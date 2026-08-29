//! 文件预览与内置编辑器。对应 `src/components/FileViewerModal.tsx`(498 行)
//! 与 `src/components/CodeEditor.tsx`(350 行),审计缺口 #29。
//!
//! # 工作区页签
//!
//! 文件树和全局搜索都通过 [`crate::workbench_area`] 打开项目级文件页签。每个页签
//! 持有独立的 [`FileViewer`]，切到文件页只隐藏终端视图，不销毁 PTY 或终端实体。
//!
//! # 编辑器是 gpui-component 的 code editor,不是自绘
//!
//! `InputState::code_editor(lang)` 自带语法高亮 / 自动缩进 / 行号 / 缩进参考线 /
//! Ctrl+F 面板(`searchable` 在 code_editor 模式下自动置真)。语言包由
//! `tree-sitter-languages` feature 提供(见 `crates/mt-app/Cargo.toml` 里那段注释):
//! 不开只有 JSON,开了 30 种。扩展名 → 语言名的映射是 [`language_for`],
//! 对照原版 `LanguageDescription.matchFilename` 覆盖的常见类型。
//!
//! # 行尾:本模块最容易漏、漏了最贵的一条
//!
//! gpui-component 的编辑器**回车永远插 `"\n"`**
//! (`input/state.rs:1159-1160` 的 `format!("\n{}", indent)`),而 `ropey::Rope`
//! 会把读进去的 `\r\n` 原样留着 —— 直接拿 `value()` 写回去,Windows 上的 CRLF 文件
//! 改一个字就变成「原有行 CRLF + 新增行 LF」的混合行尾。原版为此专门设了
//! `EditorState.lineSeparator.of('\r\n')`(`CodeEditor.tsx:242-252`)。
//!
//! 这里的等价做法是[`LineEnding`]三件套:读入时探测 → 归一成 `\n` 喂编辑器 →
//! 写回时按探测结果还原。语义与原版一致(整份文件用同一种行尾),
//! 唯一差别见 [`LineEnding::detect`] 的注释(混合行尾文件会被收敛成多数那一种)。
//!
//! # 与原版的偏差(逐条,详见各处注释)
//!
//! 1. **Markdown 里的链接点击拦不住**:gpui-component 的富文本渲染器把链接写死成
//!    `cx.open_url(&link.url)`(`text/node.rs:622`、`text/inline.rs:359`),没有回调口。
//!    于是原版三条链接处置(外链弹确认框 / 文档内锚点滚动 / 本地文件在页内跳转)
//!    都做不到,**页内跳转历史栈(`←` 返回)随之整条不做**。记档。
//! 2. **本地 HTML 是简版渲染,不是浏览器**:GPUI 侧没有 iframe 等价物,`TextView::html`
//!    与 markdown 那支是同一个富文本渲染器(无 CSS / 无 JS)。此处曾按规格 B.6.3
//!    的建议「只留源码编辑器」,**已翻案**(用户要求):现在给预览态,但配一条
//!    说明 + 工具栏常驻「用浏览器打开」——走样的排版有解释、真效果有出口,
//!    比对着一屏源码有用。相对资源不再是问题,见 [`rewrite_html_urls`]。远程
//!    HTML 属于不可信输入，只走源码编辑器，不进入富文本 HTML 渲染器。
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use futures::StreamExt;
use futures::channel::mpsc;
use futures::future::BoxFuture;
use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, ImageAssetLoader,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, Resource,
    StatefulInteractiveElement, Styled, StyledImage as _, Subscription, Task, Window, div, img,
    prelude::FluentBuilder as _, px,
};
use gpui::http_client::{
    AsyncBody, HttpClient, Request, Response, StatusCode, Url, http::HeaderValue,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::input::{Input, InputEvent, InputState, Position, Search};
use gpui_component::text::{TextView, TextViewStyle};
use markdown::{ParseOptions, mdast::Node as MarkdownNode};
use mt_project::fs::FileContentResult;
use mt_project::watch::FsWatcher;
use mt_ui::icons::FileIcon;
use mt_ui::tooltip::Tooltip;

use crate::i18n::t;
use crate::ui;

/// 文档的读写来源。远程来源持有打开时的连接快照；保存前还会与 `AppStore`
/// 中的当前连接身份复核，避免连接配置原地变化后旧页签写到错误主机。
#[derive(Clone)]
pub enum DocumentSource {
    Local {
        project_id: String,
        project_root: PathBuf,
        path: PathBuf,
    },
    Remote {
        project_id: String,
        connection: mt_config::SshConnection,
        project_root: String,
        path: PathBuf,
    },
}

impl DocumentSource {
    pub fn project_id(&self) -> &str {
        match self {
            Self::Local { project_id, .. } | Self::Remote { project_id, .. } => project_id,
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            Self::Local { path, .. } | Self::Remote { path, .. } => path,
        }
    }

    pub fn file_name(&self) -> String {
        file_name_of(&self.path().to_string_lossy()).to_string()
    }

    fn project_root_path(&self) -> PathBuf {
        match self {
            Self::Local { project_root, .. } => project_root.clone(),
            Self::Remote { project_root, .. } => PathBuf::from(project_root),
        }
    }

    fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }
}

// ─── 纯逻辑(可测) ────────────────────────────────────────────

/// `FileViewerModal.tsx:27-29` 的 `isMarkdownFile`。
pub fn is_markdown_file(path: &str) -> bool {
    has_ext(path, &["md", "markdown", "mkd", "mdx"])
}

/// `FileViewerModal.tsx:31-33` 的 `isImageFile`。
pub fn is_image_file(path: &str) -> bool {
    has_ext(
        path,
        &[
            "png", "jpg", "jpeg", "gif", "bmp", "webp", "svg", "ico", "avif", "tif", "tiff",
        ],
    )
}

/// `FileViewerModal.tsx:35-37` 的 `isHtmlFile`。
pub fn is_html_file(path: &str) -> bool {
    has_ext(path, &["html", "htm"])
}

/// 散文类文件折行,代码不折(`CodeEditor.tsx:203-206` 的 `shouldWrap`)。
pub fn should_wrap(path: &str) -> bool {
    has_ext(path, &["md", "markdown", "mkd", "mdx", "txt"])
}

/// 扩展名(小写)属于给定集合。`.tar.gz` 这类只看最后一段,与 JS 正则同口径。
fn has_ext(path: &str, exts: &[&str]) -> bool {
    let name = file_name_of(path);
    let Some((_, ext)) = name.rsplit_once('.') else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    exts.contains(&ext.as_str())
}

/// 路径的最后一段(两种分隔符都认 —— 远程/WSL 路径是 POSIX 的)。
pub fn file_name_of(path: &str) -> &str {
    let cut = path
        .rfind(['/', '\\'])
        .map(|i| i + 1)
        .unwrap_or(0);
    &path[cut..]
}

/// 两个路径指的是不是同一个文件。
///
/// **反斜杠归一 + 小写**,照抄 `FileViewerModal.tsx:277` 的 `norm` ——
/// Windows 上 notify 回来的路径大小写与盘符分隔符都可能与用户点的那一个不一致,
/// 直接比 `PathBuf` 会漏掉外部修改事件。
pub fn same_path(a: &str, b: &str) -> bool {
    fn norm(s: &str) -> String {
        s.replace('\\', "/").to_lowercase()
    }
    norm(a) == norm(b)
}

/// 文件行尾。读入时探测,写回时还原。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    /// 探测。**只要出现过一次 `\r\n` 就整份按 CRLF 处理** —— 与原版
    /// `const crlf = value.includes('\r\n')`(`CodeEditor.tsx:246`)一字不差。
    ///
    /// 混合行尾的文件因此会在保存时被收敛成 CRLF。原版在这一点上略有不同:
    /// CodeMirror 设了 `lineSeparator` 之后,孤立的 `\n` 会以控制字符留在行内容里、
    /// `doc.toString()` 原样吐回去(注释里写的「恰好暴露混合行尾」)。GPUI 侧的
    /// 编辑器没有 lineSeparator 这个概念,孤立 `\n` 只能当换行看 —— 于是保存后统一。
    /// 这是**刻意取舍**:混合行尾文件本就是坏味道,统一比留着更符合直觉,
    /// 而「一行都别动」的目标(纯 CRLF 文件保存后仍是纯 CRLF)照样达成。
    pub fn detect(text: &str) -> Self {
        if text.contains("\r\n") {
            Self::Crlf
        } else {
            Self::Lf
        }
    }
}

/// 磁盘内容 → 编辑器内容:`\r\n` 折成 `\n`。
///
/// 不归一直接喂进去也能显示(ropey 认 `\r\n` 是一个换行),但**新敲的回车是 `\n`**,
/// 于是同一份文件里两种行尾并存,还原时无从下手。归一之后「编辑器里只有 `\n`」
/// 是不变式,[`restore_line_ending`] 才能无歧义地还原。
pub fn normalize_to_lf(text: &str) -> String {
    if text.contains("\r\n") {
        text.replace("\r\n", "\n")
    } else {
        text.to_string()
    }
}

/// 编辑器内容 → 磁盘内容:按探测到的行尾还原。
///
/// 先把可能混进来的 `\r\n` 折掉再统一加 `\r`,是为了幂等 —— 免得对同一份文本
/// 调两次变成 `\r\r\n`(编辑器里理论上不该有 `\r\n`,但这条不值得赌)。
pub fn restore_line_ending(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::Crlf => normalize_to_lf(text).replace('\n', "\r\n"),
    }
}

/// 文件名 → gpui-component 的语言名(`Language::from_str` 认得的那些)。
///
/// 对照原版 `LanguageDescription.matchFilename(languages, fileName)`
/// (`CodeEditor.tsx:300`)覆盖的常见类型。认不出返回 `"text"`,落到 `Language::Plain`
/// —— 与原版「匹配不到就是纯文本」同义。
///
/// 特殊文件名(无扩展名的 `Makefile` / `Dockerfile` 之流)先于扩展名判定,
/// 与 [`mt_ui::icons::FileIcon`] 的「特殊文件名压扩展名」同一条规矩。
pub fn language_for(file_name: &str) -> &'static str {
    let name = file_name_of(file_name).to_ascii_lowercase();
    // 特殊文件名先判(有的根本没有扩展名,有的扩展名会指向错的语言:
    // `CMakeLists.txt` 的 `.txt` 什么都不是)
    match name.as_str() {
        "makefile" | "gnumakefile" => return "make",
        "cmakelists.txt" => return "cmake",
        "dockerfile" => return "bash",
        ".bashrc" | ".bash_profile" | ".zshrc" | ".profile" => return "bash",
        _ => {}
    }
    let Some((_, ext)) = name.rsplit_once('.') else {
        return "text";
    };
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" | "jsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "json" | "jsonc" => "json",
        "py" | "pyi" => "python",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "cs" => "csharp",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => "cpp",
        "css" | "scss" | "less" => "css",
        "html" | "htm" => "html",
        "sh" | "bash" | "zsh" | "fish" => "bash",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "md" | "markdown" | "mkd" | "mdx" => "markdown",
        "sql" => "sql",
        "swift" => "swift",
        "zig" => "zig",
        "ex" | "exs" => "elixir",
        "scala" | "sbt" => "scala",
        "proto" => "proto",
        "graphql" | "gql" => "graphql",
        "diff" | "patch" => "diff",
        "cmake" => "cmake",
        "ejs" => "ejs",
        "erb" => "erb",
        _ => "text",
    }
}

/// 该把光标放到第几行(1-based),`None` = 不动。
///
/// 越界不动(`CodeEditor.tsx:341` 的 `if (highlightLine > view.state.doc.lines) return`)。
pub fn highlight_target(highlight_line: Option<u32>, text: &str) -> Option<u32> {
    let line = highlight_line?;
    // 至少一行:空文件在编辑器里也是「第 1 行」
    let total = text.lines().count().max(1) as u32;
    (line >= 1 && line <= total).then_some(line)
}

/// 内容区该画哪一支。判定顺序照抄 `FileViewerModal.tsx:409-495` ——
/// **图片先于 loading**(原版图片分支压根不读文件,`useEffect` 首行就 `if (isImg) return`),
/// binary 先于 tooLarge(二进制文件的 `content` 也是空的)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Branch {
    Image,
    Loading,
    Error,
    Binary,
    TooLarge,
    Editor,
}

/// `(是图片, 在读盘, 有错, 读到的结果)` → 画哪一支。
pub fn branch_of(is_img: bool, loading: bool, has_error: bool, result: Option<&FileContentResult>) -> Branch {
    if is_img {
        return Branch::Image;
    }
    if loading {
        return Branch::Loading;
    }
    if has_error {
        return Branch::Error;
    }
    match result {
        Some(r) if r.is_binary => Branch::Binary,
        Some(r) if r.too_large => Branch::TooLarge,
        Some(_) => Branch::Editor,
        None => Branch::Loading,
    }
}

/// `canEdit = !!result && !isBinary && !tooLarge && !isImg`(`FileViewerModal.tsx:244`)。
pub fn can_edit(is_img: bool, result: Option<&FileContentResult>) -> bool {
    !is_img && matches!(result, Some(r) if !r.is_binary && !r.too_large)
}

fn supports_rich_preview(is_remote: bool, path: &str) -> bool {
    is_markdown_file(path) || (!is_remote && is_html_file(path))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RemoteRefreshFailurePresentation {
    Fatal,
    Warning,
}

fn remote_refresh_failure_presentation(
    has_loaded_result: bool,
    has_editor: bool,
) -> RemoteRefreshFailurePresentation {
    if has_loaded_result || has_editor {
        RemoteRefreshFailurePresentation::Warning
    } else {
        RemoteRefreshFailurePresentation::Fatal
    }
}

fn refresh_warning_after_remote_save(
    current: Option<String>,
    save_succeeded: bool,
) -> Option<String> {
    if save_succeeded { None } else { current }
}

/// 自己落盘的回声窗口:保存后 2s 内的 `fs-change` 不算「外部修改」
/// (`FileViewerModal.tsx:280`)。
///
/// 已知边界(原版就有,照抄不改):这 2s 内**真正的**外部修改也会被吞掉
/// (保存后立刻被 formatter / pre-commit 改写)。不改成内容比对 ——
/// 那会引入「外部改写结果恰好等于刚保存的内容」的另一类误判。
pub const ECHO_WINDOW: Duration = Duration::from_millis(2000);

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

/// GFM 表格的列对齐(分隔行的 `:---:` 语法)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MdAlign {
    Left,
    Center,
    Right,
}

/// 一张解析好的 GFM 表格。格子存**原文**,渲染时逐格走 markdown。
#[derive(Debug, PartialEq)]
struct MdTable {
    header: Vec<String>,
    aligns: Vec<MdAlign>,
    rows: Vec<Vec<String>>,
}

/// markdown 里的一张图片。纯图片段落由 AST 确认后拆出来自绘。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct MdImage {
    /// 原文里的目标,**未解码也未解析** —— 落地在 [`resolve_image_src`]
    url: String,
    alt: String,
    /// `![alt](url "title")` 的 title,悬停显示
    title: Option<String>,
    /// `[![alt](img)](link)` 外层链接:点图开外链(徽章行的写法)
    link: Option<String>,
}

#[derive(Debug, PartialEq)]
enum MdSegment {
    Text(String),
    Table(MdTable),
    /// 一整行的图片(徽章行可能并排多张)
    Images(Vec<MdImage>),
}

/// 预处理好的一块正文:`Text` 里的图片目标已改写成绝对 `file://`
/// ([`rewrite_md_image_urls`]),块顶间距([`block_top_margin`])也已算出。
///
/// 与 [`MdSegment`] 分家是因为它要**跨帧活着** —— 见 [`FileViewer::md_cache`]。
enum MdBlock {
    Text(gpui::SharedString),
    Table(MdTable),
    Images(Vec<MdImage>),
}

/// markdown 预览的分块缓存。key 是「源码 + 所在目录」,两者都没变就复用。
///
/// 有它是因为**滚动一次就是整个视图重 render 一遍**(gpui 的滚轮处理改完
/// offset 就 notify 当前 view),而 [`split_md_blocks`] 与
/// [`rewrite_md_image_urls`] 都是全文逐字符扫描 —— 一份 40 KB 的文档每帧
/// 重切一次纯属白烧。缓存的是**分块结果**,不是元素:元素每帧照建
/// (gpui 的 retained 边界在 Element 那一层,不在这里)。
struct MdCache {
    source: String,
    base_dir: PathBuf,
    local_resources: bool,
    /// `(块顶间距, 块)`。`Rc` 让 [`FileViewer::render_markdown`] 拿完就撒手,
    /// 不必攥着 `RefCell` 的借用穿过整段渲染
    blocks: Rc<Vec<(f32, MdBlock)>>,
}

/// 把 markdown 源切成**顶层 AST 块**:确认是顶层 GFM 表格或纯图片段落时才
/// 自绘，其余节点按源码范围交回 TextView。列表/引用/raw HTML/代码块整块保留，
/// 不能先按“看起来像图片的一行”拆开，否则会把容器里的代码误变成真实资源请求。
///
/// 逐块喂 TextView 而不是整篇 —— 除了表格要自绘,还有一条硬理由:
/// gpui-component 0.5.1 的非虚拟化路径把 `is_last: true` 原样传给 Root 的
/// **每个**子块(`node.rs:1150-1156`,ListState 路径才逐块算),而
/// `is_last → paragraph_gap = 0`,整篇喂进去相邻段落会贴死(用户对照原版
/// 实测)。块间距改由 [`block_top_margin`] 自己控,顺带复刻原版「标题前
/// 间距更大」的非对称节奏(`.md-preview h* { margin-top: 1.4em }`)。
fn split_md_blocks(source: &str) -> Vec<MdSegment> {
    let Ok(ast) = markdown::to_mdast(source, &ParseOptions::gfm()) else {
        return markdown_text_only(source);
    };
    // 引用、脚注与嵌套定义可能跨分段消费；遇到这些就整篇交回 TextView。
    // 仅有未被引用的顶层普通定义时可以安全分块，它本身不产生可见内容。
    if markdown_requires_shared_definition_scope(&ast) {
        return markdown_text_only(source);
    }
    let Some(children) = ast.children() else {
        return markdown_text_only(source);
    };

    let mut nodes = Vec::with_capacity(children.len());
    let mut previous_end = 0usize;
    for node in children {
        let Some(position) = node.position() else {
            return markdown_text_only(source);
        };
        let (start, end) = (position.start.offset, position.end.offset);
        if start > end || start < previous_end || source.get(start..end).is_none() {
            return markdown_text_only(source);
        }
        nodes.push((node, start, end));
        previous_end = end;
    }

    let mut segs = Vec::new();
    let mut pending_text: Option<(usize, usize)> = None;
    for (node, start, end) in nodes {
        // 未被引用的顶层定义不产生可见内容。既然上面的共享作用域检查已经确认
        // 没有引用消费者，就直接跳过，避免为它建立一个空 TextView 和块间距。
        if matches!(node, MarkdownNode::Definition(_)) {
            continue;
        }
        let raw = &source[start..end];
        let custom = match node {
            MarkdownNode::Table(_) => {
                parse_table_block(raw).map(|table| vec![MdSegment::Table(table)])
            }
            MarkdownNode::Paragraph(_) => split_top_level_image_paragraph(node),
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
    segs
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
enum MdImageSrc {
    /// 本地文件(相对路径已按当前文件所在目录解析)
    Local(PathBuf),
    /// 远程图片:字节由 [`PreviewHttpClient`] 拉回来(见 [`FileViewer::render_md_remote_image`])
    Remote(String),
    /// `data:` / 认不出的 scheme
    Unsupported,
}

/// 图片 URL → 落点。相对路径按**当前 md 文件所在目录**解析,与原版
/// `resolveImgSrc`(`FileViewerModal.tsx:145-150` 的
/// `convertFileSrc(fileDir + '/' + src)`)同一口径。
fn resolve_image_src(url: &str, base_dir: &Path) -> MdImageSrc {
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
fn to_file_url(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(|url| url.to_string())
}

/// 预览器的 HTTP 客户端,装在 `main` 里(`cx.set_http_client`)。
///
/// gpui 默认那份是 `NullHttpClient`(`gpui/app.rs:2343`,`send()` 直接报错),
/// 而 gpui-component 的富文本渲染器把图片一律画成 `img(SharedUri)`
/// (`text/node.rs:609`)—— URI 走的就是 http client。于是预览里的图片全靠这条路:
///
/// - `file://`:本地图片。md / html 源里的相对路径在渲染前被改写成绝对 file URL
///   ([`rewrite_md_image_urls`] / [`rewrite_html_urls`]),到这里读盘返回。
/// - `http(s)://`:网络图片(README 顶上的徽章、外链截图)。`reqwest::blocking`
///   拉回来 —— 与价格表那条链同一个客户端库(见 `pricing::fetch_models_dev`)。
///
/// 其余 scheme 一律拒绝。本地资源只读普通文件并限制 32MB；出网资源另有 10s
/// 超时(`reqwest::blocking` 默认无限等)，同样限制 32MB。详见
/// [`fetch_local_preview_bytes`] / [`fetch_remote_bytes`]。
///
/// 这是进程级客户端，不只服务文件页；其它富文本入口必须先走对应安全策略。
/// AI 会话正文统一经 [`sanitize_session_markdown`] 禁掉全部自动资源请求。
pub struct PreviewHttpClient;

const PREVIEW_IMAGE_MAX_BYTES: u64 = 32 * 1024 * 1024;

impl HttpClient for PreviewHttpClient {
    fn type_name(&self) -> &'static str {
        "PreviewHttpClient"
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    /// 代理由 `reqwest` 自己按环境变量认(`HTTP_PROXY` / `HTTPS_PROXY`),
    /// 这里不额外指定 —— gpui 只拿它做展示,不参与请求构造。
    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let uri = req.uri().to_string();
        Box::pin(async move {
            let url = Url::parse(&uri).with_context(|| format!("URL 解析失败: {uri}"))?;
            // 读盘与出网都是**阻塞**的,但这条 future 由 gpui 的 asset 系统
            // 丢在后台执行器上跑,不落主线程
            let bytes = match url.scheme() {
                "file" => {
                    let path = url
                        .to_file_path()
                        .map_err(|_| anyhow::anyhow!("不是本地文件路径: {uri}"))?;
                    fetch_local_preview_bytes(&path)?
                }
                "http" | "https" => fetch_remote_bytes(&uri)?,
                other => anyhow::bail!("预览不支持的协议 {other}: {uri}"),
            };
            Ok(Response::builder()
                .status(StatusCode::OK)
                .body(AsyncBody::from(bytes))?)
        })
    }
}

/// 本地富文本资源只读取普通文件，并与网络资源共用 32MB 硬上限。先 canonicalize
/// 再检查可同时允许“项目里的图片符号链接”并拒绝设备、FIFO 与目录；打开后再检查
/// 一次并限量读取，避免路径替换或文件增长绕过预检。
fn fetch_local_preview_bytes(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;

    let canonical =
        std::fs::canonicalize(path).with_context(|| format!("读不到 {}", path.display()))?;
    let metadata = std::fs::symlink_metadata(&canonical)
        .with_context(|| format!("无法检查 {}", canonical.display()))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "预览资源不是普通文件: {}",
        canonical.display()
    );
    anyhow::ensure!(
        metadata.len() <= PREVIEW_IMAGE_MAX_BYTES,
        "预览资源过大({} 字节): {}",
        metadata.len(),
        canonical.display()
    );

    let file = std::fs::File::open(&canonical)
        .with_context(|| format!("读不到 {}", canonical.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("无法检查 {}", canonical.display()))?;
    anyhow::ensure!(
        opened.is_file(),
        "预览资源打开后不再是普通文件: {}",
        canonical.display()
    );
    anyhow::ensure!(
        opened.len() <= PREVIEW_IMAGE_MAX_BYTES,
        "预览资源过大({} 字节): {}",
        opened.len(),
        canonical.display()
    );

    let mut body = Vec::with_capacity(opened.len() as usize);
    file.take(PREVIEW_IMAGE_MAX_BYTES + 1)
        .read_to_end(&mut body)?;
    anyhow::ensure!(
        body.len() as u64 <= PREVIEW_IMAGE_MAX_BYTES,
        "预览资源读取时超过大小上限: {}",
        canonical.display()
    );
    Ok(body)
}

/// 一次 GET,把响应体整个读回来。**阻塞**,只许在后台执行器上调 —— gpui 的
/// asset 系统正是这么跑的(`app.rs:2018` 的 `background_executor().spawn`)。
///
/// 客户端存成进程级单例:每次请求现建一个要重做 TLS 栈初始化,而 README 顶上
/// 一排徽章就是一串并发请求。
///
/// ⚠️ 已知取舍:每个请求会占住一个后台线程直到超时,一屏全是拉不动的远程图片时
/// (离线 / 墙)线程池会被占满 10s。超时因此压得比价格表那条链(15s)短 ——
/// 图片拉不回来只是少一张图,不值得把后台线程按住更久。
fn fetch_remote_bytes(url: &str) -> anyhow::Result<Vec<u8>> {
    /// 徽章服务(shields.io 之流)对没有 UA 的请求有的直接 403
    const UA: &str = concat!("mini-term/", env!("CARGO_PKG_VERSION"));
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    use std::io::Read as _;

    let client = CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(UA)
            .build()
            .unwrap_or_default()
    });
    let resp = client.get(url).send()?;
    anyhow::ensure!(
        resp.status().is_success(),
        "HTTP {} — {url}",
        resp.status().as_u16()
    );
    // content-length 可能缺席(chunked),所以读的时候再兜一次上限
    if let Some(len) = resp.content_length() {
        anyhow::ensure!(
            len <= PREVIEW_IMAGE_MAX_BYTES,
            "图片过大({len} 字节): {url}"
        );
    }
    let mut body = Vec::new();
    resp.take(PREVIEW_IMAGE_MAX_BYTES + 1)
        .read_to_end(&mut body)?;
    anyhow::ensure!(
        body.len() as u64 <= PREVIEW_IMAGE_MAX_BYTES,
        "图片过大: {url}"
    );
    Ok(body)
}

/// 把 md 源里图片的**本地**目标改写成 `file:///…` 绝对 URL。
///
/// 整行只有图片的那些行由 [`FileViewer::render_md_images`] 自绘、不经过这里;
/// 这条是给**内联**图片兜底的(列表项 `- ![a](b)`、引用块、表格格子里的图片)——
/// 它们要走 TextView,而那条路只认网络 URI,配上 [`PreviewHttpClient`] 才画得出来。
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

fn rewrite_md_image_urls(source: &str, base_dir: &Path) -> String {
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

fn remote_markdown_url_allowed(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with('#')
}

#[derive(Debug)]
struct MarkdownReplacement {
    start: usize,
    end: usize,
    value: String,
}

const MAX_UNTRUSTED_MARKDOWN_SANITIZE_PASSES: usize = 4;

fn markdown_plain_text(node: &MarkdownNode) -> String {
    match node {
        MarkdownNode::Image(image) => image.alt.clone(),
        MarkdownNode::ImageReference(image) => image.alt.clone(),
        _ => node
            .children()
            .map(|children| {
                children.iter().fold(String::new(), |mut text, child| {
                    text.push_str(&markdown_plain_text(child));
                    text
                })
            })
            .unwrap_or_else(|| node.to_string()),
    }
}

fn markdown_safe_plain_label(value: &str, fallback: &str) -> String {
    if value.trim().is_empty() {
        return fallback.into();
    }
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn markdown_replacement(node: &MarkdownNode, value: String) -> Option<MarkdownReplacement> {
    let position = node.position()?;
    Some(MarkdownReplacement {
        start: position.start.offset,
        end: position.end.offset,
        value,
    })
}

/// Turn an untrusted raw-HTML AST node into visible Markdown text. Escape every
/// ASCII punctuation character so a second GFM parse cannot recreate either an
/// `mdast::Html` node or Markdown links/images hidden inside an attribute value.
/// Backslash escapes render as the original punctuation, preserving readable
/// source without giving the replacement any active Markdown syntax.
fn inert_markdown_html_source(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_punctuation() {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn collect_untrusted_markdown_replacements(
    node: &MarkdownNode,
    replacements: &mut Vec<MarkdownReplacement>,
) {
    match node {
        MarkdownNode::Link(link) if !remote_markdown_url_allowed(&link.url) => {
            let label = markdown_safe_plain_label(&markdown_plain_text(node), "link");
            if let Some(replacement) = markdown_replacement(node, label) {
                replacements.push(replacement);
            }
            return;
        }
        MarkdownNode::Image(image) => {
            let alt = markdown_safe_plain_label(&image.alt, "image");
            if let Some(replacement) = markdown_replacement(node, alt) {
                replacements.push(replacement);
            }
            return;
        }
        MarkdownNode::ImageReference(image) => {
            let alt = markdown_safe_plain_label(&image.alt, "image");
            if let Some(replacement) = markdown_replacement(node, alt) {
                replacements.push(replacement);
            }
            return;
        }
        MarkdownNode::Definition(definition) if !remote_markdown_url_allowed(&definition.url) => {
            if let Some(replacement) = markdown_replacement(node, String::new()) {
                replacements.push(replacement);
            }
            return;
        }
        MarkdownNode::Html(html) => {
            if let Some(replacement) =
                markdown_replacement(node, inert_markdown_html_source(&html.value))
            {
                replacements.push(replacement);
            }
            return;
        }
        _ => {}
    }

    if let Some(children) = node.children() {
        for child in children {
            collect_untrusted_markdown_replacements(child, replacements);
        }
    }
}

#[cfg(test)]
fn collect_remote_markdown_replacements(
    node: &MarkdownNode,
    replacements: &mut Vec<MarkdownReplacement>,
) {
    collect_untrusted_markdown_replacements(node, replacements);
}

fn markdown_as_indented_code(source: &str) -> String {
    source
        .split('\n')
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn apply_markdown_replacements(source: &str, mut replacements: Vec<MarkdownReplacement>) -> String {
    replacements.sort_unstable_by_key(|replacement| std::cmp::Reverse(replacement.start));

    let mut sanitized = source.to_string();
    let mut next_start = source.len();
    for replacement in replacements {
        if replacement.end > next_start
            || replacement.start > replacement.end
            || source.get(replacement.start..replacement.end).is_none()
        {
            continue;
        }
        sanitized.replace_range(replacement.start..replacement.end, &replacement.value);
        next_start = replacement.start;
    }
    sanitized
}

/// Keep reparsing transformed Markdown until the renderer's own GFM grammar
/// sees no disallowed nodes. Escaping an HTML block can change the following
/// indented block into active Markdown, so a single AST generation is not a
/// sufficient security boundary.
fn sanitize_untrusted_markdown_with_pass_limit(source: &str, pass_limit: usize) -> String {
    let mut sanitized = source.to_string();
    for _ in 0..pass_limit {
        let Ok(ast) = markdown::to_mdast(&sanitized, &ParseOptions::gfm()) else {
            return markdown_as_indented_code(source);
        };
        let mut replacements = Vec::new();
        collect_untrusted_markdown_replacements(&ast, &mut replacements);
        if replacements.is_empty() {
            return sanitized;
        }
        sanitized = apply_markdown_replacements(&sanitized, replacements);
    }

    // A transformed document is safe to render only after reparsing proves it
    // has no active replacements. If the bounded loop cannot establish that
    // fixed point, keep the original source visible as inert code.
    markdown_as_indented_code(source)
}

fn sanitize_untrusted_markdown(source: &str) -> String {
    sanitize_untrusted_markdown_with_pass_limit(source, MAX_UNTRUSTED_MARKDOWN_SANITIZE_PASSES)
}

/// Remote rich-text is untrusted input from another machine. Parse with the
/// same GFM AST used by `TextView::markdown`, then replace disallowed links,
/// images, and reference definitions by source byte range. Every real raw-HTML
/// node becomes visible inert source; AST positions keep fenced/indented/inline
/// code byte-for-byte out of scope.
fn sanitize_remote_markdown(source: &str) -> String {
    sanitize_untrusted_markdown(source)
}

/// AI session logs are untrusted rich text and share the process-wide preview
/// HTTP client. Preserve Markdown formatting and explicit Markdown links, turn
/// every image into plain alt text, and make raw HTML visible but inert so
/// opening a history entry cannot read local files or issue background network
/// requests.
pub fn sanitize_session_markdown(source: &str) -> String {
    sanitize_untrusted_markdown(source)
}

/// 把 HTML 源里 `src` / `href` / `poster` 的**本地**目标改写成 `file:///…`。
///
/// 逐条对照原版 `htmlSrcDoc`(`FileViewerModal.tsx:134-143`)那条正则,排除清单
/// 也一样(http(s) / data / blob / mailto / tel / `#` / javascript)。原版靠
/// `convertFileSrc` 转 asset 协议,这里转 `file://` 交给 [`PreviewHttpClient`]。
fn rewrite_html_urls(source: &str, base_dir: &Path) -> String {
    // 大小写不敏感的定位副本。`to_ascii_lowercase` 只动 ASCII,**字节长度不变**,
    // 索引因此能直接拿回原文切片(`to_lowercase` 就不行,有字符会变长)
    let lower = source.to_ascii_lowercase();
    let mut out = String::with_capacity(source.len());
    let mut pos = 0usize;
    for attr in html_url_attributes(&lower, false) {
        out.push_str(&source[pos..attr.value_start]);
        out.push_str(&rewrite_html_value(
            &source[attr.value_start..attr.value_end],
            base_dir,
        ));
        pos = attr.value_end;
    }
    out.push_str(&source[pos..]);
    out
}

#[derive(Clone, Copy)]
struct HtmlUrlAttribute {
    value_start: usize,
    value_end: usize,
    #[cfg(test)]
    name: &'static str,
}

fn skip_html_tag(lower: &str, cursor: usize) -> usize {
    let bytes = lower.as_bytes();
    bytes[cursor..]
        .iter()
        .position(|byte| *byte == b'>')
        .map(|relative| cursor + relative + 1)
        .unwrap_or(bytes.len())
}

fn skip_html_end_tag(lower: &str, mut cursor: usize) -> usize {
    let bytes = lower.as_bytes();
    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }
        if bytes[cursor] == b'>' {
            return cursor + 1;
        }
        if bytes[cursor] == b'/' {
            cursor += 1;
            continue;
        }

        let name_start = cursor;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b'=' | b'/' | b'>' | b'"' | b'\'' | b'<')
        {
            cursor += 1;
        }
        if cursor == name_start {
            // 关闭标签里的孤立引号只是解析错误，不会让后面的 `>` 失去结束作用。
            cursor += 1;
            continue;
        }
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        match bytes.get(cursor).copied() {
            Some(quote @ (b'"' | b'\'')) => {
                cursor += 1;
                while cursor < bytes.len() && bytes[cursor] != quote {
                    cursor += 1;
                }
                if cursor < bytes.len() {
                    cursor += 1;
                }
            }
            Some(_) => {
                while cursor < bytes.len()
                    && !bytes[cursor].is_ascii_whitespace()
                    && bytes[cursor] != b'>'
                {
                    cursor += 1;
                }
            }
            None => break,
        }
    }
    bytes.len()
}

fn skip_html_comment(lower: &str, mut cursor: usize) -> usize {
    let bytes = lower.as_bytes();
    // HTML5 的 abrupt-closing empty comment：`<!-->` / `<!--->`。
    if bytes.get(cursor) == Some(&b'>') {
        return cursor + 1;
    }
    if bytes[cursor..].starts_with(b"->") {
        return cursor + 2;
    }
    while cursor < bytes.len() {
        if bytes[cursor..].starts_with(b"-->") {
            return cursor + 3;
        }
        if bytes[cursor..].starts_with(b"--!>") {
            return cursor + 4;
        }
        cursor += 1;
    }
    bytes.len()
}

fn is_raw_text_tag(tag: &str) -> bool {
    matches!(
        tag,
        "script"
            | "style"
            | "textarea"
            | "title"
            | "xmp"
            | "iframe"
            | "noembed"
            | "noframes"
            | "plaintext"
    )
}

fn skip_raw_text_element(lower: &str, mut cursor: usize, tag: &str) -> usize {
    if tag == "plaintext" {
        return lower.len();
    }
    let needle = format!("</{tag}");
    while let Some(relative) = lower[cursor..].find(&needle) {
        let close_start = cursor + relative;
        let name_end = close_start + needle.len();
        let boundary = lower.as_bytes().get(name_end).copied();
        if boundary.is_none_or(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>')) {
            return skip_html_end_tag(lower, name_end);
        }
        cursor = name_end;
    }
    lower.len()
}

/// 收集真实开始标签里的 `src=` / `href=` / `poster=` 值区间。HTML 允许属性值
/// 不加引号，也会恢复 `<img/src=x>` 与 `alt="x"src=y` 这类错误写法；同时必须
/// 跳过普通文本、注释和 HTML namespace 的 raw-text 内容，避免把示例代码当成
/// 属性；svg/math foreign content 则保守继续扫描，防止 namespace 恢复产生
/// 活动图片。
fn html_url_attributes(
    lower: &str,
    fail_closed_after_foreign_content: bool,
) -> Vec<HtmlUrlAttribute> {
    let bytes = lower.as_bytes();
    let mut attrs = Vec::new();
    let mut pos = 0usize;
    // foreign-content 的 tree-builder 恢复规则无法只靠词法标签栈精确复刻。
    // 不可信清洗启用 fail-closed 时，一旦见过非自闭合 svg/math，后续都不再
    // 跳过 raw-text，宁可多清洗也不能漏活动图片；可信本地改写不启用这条
    // 策略。
    let mut saw_foreign_content = false;

    while pos < bytes.len() {
        let Some(relative) = lower[pos..].find('<') else {
            break;
        };
        let open = pos + relative;
        let mut cursor = open + 1;
        if lower[cursor..].starts_with("!--") {
            pos = skip_html_comment(lower, cursor + 3);
            continue;
        }
        let Some(first) = bytes.get(cursor).copied() else {
            break;
        };
        if first == b'/' {
            let mut name_cursor = cursor + 1;
            while name_cursor < bytes.len()
                && !bytes[name_cursor].is_ascii_whitespace()
                && !matches!(bytes[name_cursor], b'/' | b'>')
            {
                name_cursor += 1;
            }
            pos = skip_html_end_tag(lower, name_cursor);
            continue;
        }
        if matches!(first, b'!' | b'?') {
            pos = skip_html_tag(lower, cursor + 1);
            continue;
        }
        if !first.is_ascii_alphabetic() {
            pos = cursor;
            continue;
        }

        let tag_start = cursor;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && !matches!(bytes[cursor], b'/' | b'>')
        {
            cursor += 1;
        }
        let tag = &lower[tag_start..cursor];
        let mut self_closing = false;

        while cursor < bytes.len() {
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if cursor >= bytes.len() {
                break;
            }
            if bytes[cursor] == b'>' {
                cursor += 1;
                break;
            }
            if bytes[cursor] == b'/' {
                if bytes.get(cursor + 1) == Some(&b'>') {
                    self_closing = true;
                    cursor += 2;
                    break;
                }
                // html5ever 接受 `<img/src=x>`；单独的 `/` 当属性分隔符跳过。
                cursor += 1;
                continue;
            }

            let name_start = cursor;
            while cursor < bytes.len()
                && !bytes[cursor].is_ascii_whitespace()
                && !matches!(bytes[cursor], b'=' | b'/' | b'>' | b'"' | b'\'' | b'<')
            {
                cursor += 1;
            }
            if cursor == name_start {
                cursor += 1;
                continue;
            }
            let name = match &lower[name_start..cursor] {
                "src" => Some("src"),
                "href" => Some("href"),
                "poster" => Some("poster"),
                _ => None,
            };

            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            if bytes.get(cursor) != Some(&b'=') {
                continue;
            }
            cursor += 1;
            while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }

            let (value_start, value_end) = match bytes.get(cursor).copied() {
                Some(quote @ (b'"' | b'\'')) => {
                    cursor += 1;
                    let value_start = cursor;
                    while cursor < bytes.len() && bytes[cursor] != quote {
                        cursor += 1;
                    }
                    let value_end = cursor;
                    if cursor < bytes.len() {
                        cursor += 1;
                    }
                    (value_start, value_end)
                }
                Some(_) => {
                    let value_start = cursor;
                    while cursor < bytes.len()
                        && !bytes[cursor].is_ascii_whitespace()
                        && bytes[cursor] != b'>'
                    {
                        cursor += 1;
                    }
                    (value_start, cursor)
                }
                None => (cursor, cursor),
            };
            if let Some(_name) = name {
                attrs.push(HtmlUrlAttribute {
                    value_start,
                    value_end,
                    #[cfg(test)]
                    name: _name,
                });
            }
        }

        pos = if (!fail_closed_after_foreign_content || !saw_foreign_content)
            && is_raw_text_tag(tag)
        {
            skip_raw_text_element(lower, cursor, tag)
        } else {
            cursor.max(open + 1)
        };
        if fail_closed_after_foreign_content && !self_closing && matches!(tag, "svg" | "math") {
            saw_foreign_content = true;
        }
    }

    attrs
}

/// Historical lexical sanitizer retained for focused regression tests. It is
/// not a security boundary: untrusted Markdown raw HTML is made inert by AST
/// replacement, and standalone remote HTML never enters the rich renderer.
#[cfg(test)]
fn sanitize_untrusted_html_urls(
    source: &str,
    allow_external_links: bool,
    allow_external_resources: bool,
) -> String {
    let lower = source.to_ascii_lowercase();
    let mut out = String::with_capacity(source.len());
    let mut pos = 0usize;
    for attr in html_url_attributes(&lower, true) {
        let value = source[attr.value_start..attr.value_end].trim();
        let value_lower = value.to_ascii_lowercase();
        let is_web = ["http:", "https:"]
            .iter()
            .any(|prefix| value_lower.starts_with(prefix));
        let replacement = match attr.name {
            "href" if allow_external_links && value.starts_with('#') => {
                &source[attr.value_start..attr.value_end]
            }
            "href"
                if allow_external_links
                    && (is_web
                        || ["mailto:", "tel:"]
                            .iter()
                            .any(|prefix| value_lower.starts_with(prefix))) =>
            {
                &source[attr.value_start..attr.value_end]
            }
            "src" | "poster" if allow_external_resources && is_web => {
                &source[attr.value_start..attr.value_end]
            }
            "href" => "#",
            _ => "about:blank",
        };
        out.push_str(&source[pos..attr.value_start]);
        out.push_str(replacement);
        pos = attr.value_end;
    }
    out.push_str(&source[pos..]);
    out
}

/// Legacy test helper for the former remote-HTML preview path.
#[cfg(test)]
fn sanitize_remote_html_urls(source: &str) -> String {
    sanitize_untrusted_html_urls(source, true, false)
}

/// 一个属性值:本地目标转 `file://`,其余原样(排除清单同原版正则)。
fn rewrite_html_value(value: &str, base_dir: &Path) -> String {
    const SKIP: [&str; 8] = [
        "http:",
        "https:",
        "data:",
        "blob:",
        "mailto:",
        "tel:",
        "javascript:",
        "file:",
    ];
    let target = value.trim();
    let lower = target.to_ascii_lowercase();
    if target.is_empty() || target.starts_with('#') || SKIP.iter().any(|p| lower.starts_with(p)) {
        return value.to_string();
    }
    match resolve_image_src(target, base_dir) {
        MdImageSrc::Local(path) => to_file_url(&path).unwrap_or_else(|| value.to_string()),
        _ => value.to_string(),
    }
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

/// 块顶间距:对照 `.md-preview` 的纵向节奏 —— 段落间 `p { margin: 0.8em }`
/// (相邻外边距在 CSS 里折叠,取 0.8em ≈ 11px);标题前 `margin-top: 1.4em`
/// (≈20px,原版按标题自身字号算,这里取 h2/h3 档的近似);表格 `margin: 1em`
/// (≈13px)。首块为 0,标题后的间距由**下一块**的 11px 承担(原版 0.6em≈10px)。
fn block_top_margin(ix: usize, seg: &MdSegment) -> f32 {
    if ix == 0 {
        return 0.0;
    }
    match seg {
        // 图片与表格同档:原版 `.md-preview img` 吃 p 的 0.8em,块级化之后
        // 按「独立块」给 1em(≈13px),与表格一致
        MdSegment::Table(_) | MdSegment::Images(_) => 13.0,
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
fn column_weights(table: &MdTable) -> Vec<f32> {
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
/// **表格自绘的代价全压在这一个判定上。** 每个格子一个 [`TextView::markdown`],
/// 而 [`FileViewer::render_markdown`] 的滚动容器是非虚拟化的普通 div ——
/// gpui 的滚轮处理改完 offset 就 `cx.notify(current_view)`
/// (`gpui::elements::div` 里那条),于是**滚一格 = 整篇重建一遍,视口外的表格
/// 也不例外**。实测一份 26 张表的需求文档是每帧 1425 个 TextView(每个还各带
/// 一个 focus handle 进 dispatch tree),滚动直接卡死;同等体量、只有 1 张表的
/// 文档 89 个,毫无问题 —— 差的不是文件大小,是格子数。
///
/// 判据**保守到底**:只要出现任何可能被 markdown 当标记的字符就判否,宁可多起
/// 一个 TextView,也不能把行内 code / 加粗 / 链接画成源码。放行的格子渲染结果
/// 与走 TextView **逐像素一致** —— 组件的普通文本 run 直接吃
/// `window.text_style()`(`text/inline.rs:247-259`),字号颜色行高全靠继承,
/// 与这里的纯文本元素同源;连「多个空格折叠成一个」那点差别也靠下面那条挡掉。
fn is_plain_cell(s: &str) -> bool {
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

/// 一个格子的内容元素:纯文字走快路,其余仍逐格按 markdown 渲染。
fn render_md_cell(
    seg_ix: usize,
    row_ix: usize,
    col_ix: usize,
    cell: &str,
    style: &TextViewStyle,
    window: &mut Window,
    cx: &mut App,
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
        window,
        cx,
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
/// `size()` 给的是**设备像素**。svg 那条路 gpui 按 `SMOOTH_SVG_SCALE_FACTOR`
/// 放大后光栅化(`elements/img.rs:696-706`),换算回逻辑像素要除回去 —— 那个常量
/// 没从 gpui 导出(私有 mod + `use`,不是 `pub use`),只能照抄它的值 2.0。
fn image_display_width(data: &gpui::RenderImage, is_svg: bool, avail_w: f32) -> f32 {
    let scale = if is_svg { 2.0 } else { 1.0 };
    (data.size(0).width.0 as f32 / scale).clamp(1.0, avail_w.max(1.0))
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
        .when_some(hint, |el, hint| {
            el.tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
        })
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

// ─── 视图 ─────────────────────────────────────────────────────

pub struct FileViewer {
    source: DocumentSource,
    project_root: PathBuf,
    current_path: PathBuf,
    highlight_line: Option<u32>,

    loading: bool,
    remote_refreshing: bool,
    error: Option<String>,
    /// A re-activation refresh failed after usable content was already loaded.
    /// Keep it separate from `error`: the latter selects the full-page fatal
    /// branch, while this warning must leave the editor/draft visible.
    refresh_warning: Option<String>,
    result: Option<FileContentResult>,
    /// 编辑器实体。**换文件 / 显式重载才重建** —— `set_value` 会清撤销栈,
    /// 「预览 ↔ 源码」来回切只是不画它,草稿与撤销栈都留着
    /// (原版 `className={preview ? 'hidden' : 'h-full'}`,只隐藏不卸载)。
    editor: Option<Entity<InputState>>,
    /// 磁盘上最后一次已知内容(已归一成 `\n`)。载入 / 保存成功时更新。
    saved: String,
    /// 磁盘现内容的投影(Markdown 预览渲染用它,不用 `result.content` ——
    /// 后者是「打开时」的内容,保存后就旧了)。
    disk: String,
    /// 切到预览那一刻的草稿快照;`None` = 干净,预览直接用 [`Self::disk`]。
    preview_draft: Option<String>,
    /// 文件读进来时的行尾。写回时按它还原(见模块注释)。
    line_ending: LineEnding,
    /// markdown 预览的分块缓存,见 [`MdCache`]。`RefCell` 是因为
    /// [`Self::render_markdown`] 只拿得到 `&self`(gpui 的 `Render::render`
    /// 之下全是不可变借用),而这份缓存要在渲染途中回填。
    md_cache: RefCell<Option<MdCache>>,
    /// 远程 Markdown 图片按文档、按 URL 记录用户明确批准。未命中时只能画
    /// 占位，绝不能把 URI 交给进程级图片加载器。
    approved_remote_images: HashSet<String>,

    preview: bool,
    dirty: bool,
    saving: bool,
    save_error: Option<String>,
    save_warning: Option<String>,
    ext_changed: bool,
    last_save_at: Option<Instant>,

    /// 远程可编辑文件的加载/上次保存基线。二进制、超限和失败分支为 `None`。
    remote_baseline: Option<crate::remote_ssh::RemoteFileBaseline>,
    /// 保存前发现远端已变化。保留后端返回的新内容，让“重新加载”无需第二次网络请求。
    remote_conflict: Option<crate::remote_ssh::RemoteFileReadResult>,
    /// 当前配置中的 SSH 连接身份已与打开页签时不同；此页签只允许查看，不允许保存。
    remote_source_invalid: bool,
    load_generation: u64,

    watcher: Arc<FsWatcher>,
    watched: Option<PathBuf>,

    focus: FocusHandle,
    _fs_task: Task<()>,
    _editor_sub: Option<Subscription>,
}

impl FileViewer {
    pub fn new_document(
        source: DocumentSource,
        highlight_line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new(source, highlight_line, window, cx)
    }

    fn new(
        source: DocumentSource,
        highlight_line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // notify 自己的线程只把「哪个文件变了」丢过来,判定在主线程做
        let (tx, mut rx) = mpsc::unbounded::<PathBuf>();
        let watcher = Arc::new(FsWatcher::new(move |change| {
            let _ = tx.unbounded_send(change.path);
        }));
        // `spawn_in` 而不是 `spawn`:重载要建 `InputState`,那是 `&mut Window` 的活
        let fs_task = cx.spawn_in(window, async move |this, cx| {
            while let Some(path) = rx.next().await {
                if this
                    .update_in(cx, |view: &mut FileViewer, window, cx| {
                        view.on_fs_change(&path, window, cx)
                    })
                    .is_err()
                {
                    return;
                }
            }
        });

        let project_root = source.project_root_path();
        let path = source.path().to_path_buf();
        let mut this = Self {
            source,
            project_root,
            current_path: path,
            highlight_line,
            loading: false,
            remote_refreshing: false,
            error: None,
            refresh_warning: None,
            result: None,
            editor: None,
            saved: String::new(),
            disk: String::new(),
            preview_draft: None,
            line_ending: LineEnding::Lf,
            md_cache: RefCell::new(None),
            approved_remote_images: HashSet::new(),
            // 文件树打开 Markdown / HTML 时默认看渲染稿；内容搜索带行号时切到
            // 源码，否则命中光标虽然已经定位，用户看到的仍是无法对应行号的预览。
            preview: highlight_line.is_none(),
            dirty: false,
            saving: false,
            save_error: None,
            save_warning: None,
            ext_changed: false,
            last_save_at: None,
            remote_baseline: None,
            remote_conflict: None,
            remote_source_invalid: false,
            load_generation: 0,
            watcher,
            watched: None,
            focus: cx.focus_handle(),
            _fs_task: fs_task,
            _editor_sub: None,
        };
        this.reload(window, cx);
        this
    }

    fn path_str(&self) -> String {
        self.current_path.to_string_lossy().to_string()
    }

    pub fn file_name(&self) -> String {
        let p = self.path_str();
        file_name_of(&p).to_string()
    }

    fn is_img(&self) -> bool {
        is_image_file(&self.path_str())
    }

    fn renders_local_image(&self) -> bool {
        !self.source.is_remote() && self.is_img()
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// 「预览 / 源码」段控件的显示条件:Markdown 始终允许，本地 HTML 允许，
    /// 远程 HTML 只走源码；最后都必须满足 `canEdit`。
    ///
    /// 本地 HTML 那一半曾经被摘掉(模块注释偏差 2 的旧结论:没有 iframe 等价物,
    /// 富文本渲染器画出来的东西「比不提供更误导人」)。现在**改为提供** ——
    /// 见 [`Self::render_html`]:简版渲染 + 顶上一条说明 + 工具栏常驻
    /// 「用浏览器打开」,把真效果的出口摆明,比只给一屏源码有用。
    fn has_preview_toggle(&self) -> bool {
        let path = self.path_str();
        supports_rich_preview(self.source.is_remote(), &path)
            && can_edit(self.renders_local_image(), self.result.as_ref())
    }

    // ── 读盘 ──────────────────────────────────────────────

    /// 读当前文件并重建编辑器。图片分支不读盘(原版 `if (!open || isImg) return`)。
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 保存任务已经拿到旧基线并可能正在落盘。此时重建编辑器会让迟到的保存
        // 完成跨代修改状态，也会允许用户在旧写入尚未结束时启动第二次保存。
        if self.saving {
            return;
        }
        self.remote_refreshing = false;
        self.refresh_warning = None;
        self.rewatch();
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        self.remote_conflict = None;
        self.remote_baseline = None;
        if self.is_img() {
            self.loading = false;
            self.result = None;
            self.editor = None;
            self._editor_sub = None;
            cx.notify();
            return;
        }
        self.loading = true;
        self.error = None;
        self.result = None;
        self.editor = None;
        self._editor_sub = None;
        cx.notify();

        let path = self.current_path.clone();
        match self.source.clone() {
            DocumentSource::Local { project_root, .. } => {
                cx.spawn_in(window, async move |this, cx| {
                    // 读盘是阻塞的,**不能在主线程上跑**
                    let probe = (project_root, path.clone());
                    let outcome = cx
                        .background_executor()
                        .spawn(async move { mt_project::fs::read_file_content(&probe.0, &probe.1) })
                        .await;
                    let _ = this.update_in(cx, |view: &mut FileViewer, window, cx| {
                        if view.current_path != path || view.load_generation != generation {
                            return;
                        }
                        view.loading = false;
                        match outcome {
                            Ok(res) => view.apply_content(res, window, cx),
                            Err(err) => {
                                view.error = Some(format!("{err:#}"));
                                cx.notify();
                            }
                        }
                    });
                })
                .detach();
            }
            DocumentSource::Remote {
                connection,
                project_root,
                ..
            } => {
                let remote_path = path.to_string_lossy().into_owned();
                cx.spawn_in(window, async move |this, cx| {
                    let outcome = cx
                        .background_executor()
                        .spawn(async move {
                            crate::remote_ssh::read_file_content(
                                &connection,
                                &project_root,
                                &remote_path,
                            )
                        })
                        .await;
                    let _ = this.update_in(cx, |view: &mut FileViewer, window, cx| {
                        if view.current_path != path || view.load_generation != generation {
                            return;
                        }
                        view.loading = false;
                        match outcome {
                            Ok(content) => {
                                view.apply_remote_content(content, window, cx);
                            }
                            Err(err) => {
                                view.error = Some(err);
                                cx.notify();
                            }
                        }
                    });
                })
                .detach();
            }
        }
    }

    /// 内容到位:落基线 + 建编辑器。
    ///
    /// 「编辑基线与内容一起落位」是原版注释里点名的一条(`FileViewerModal.tsx:224`)——
    /// 分两步会出现「内容已换、基线还是旧文件」的窗口,那一瞬间的脏态是错的。
    fn apply_content(
        &mut self,
        res: FileContentResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remote_baseline = None;
        self.remote_conflict = None;
        self.apply_file_content(res, window, cx);
    }

    fn apply_remote_content(
        &mut self,
        content: crate::remote_ssh::RemoteFileReadResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.validate_remote_source(cx);
        if self.remote_source_invalid {
            if self.result.is_none() {
                self.error = Some(t("fileViewer", "remoteConnectionChanged").to_string());
            }
            cx.notify();
            return;
        }
        self.remote_baseline = content.baseline;
        self.remote_conflict = None;
        self.apply_file_content(content.content, window, cx);
    }

    /// Re-activation refresh for a clean remote tab. Keep the existing editor
    /// entity (and therefore cursor/undo history) when the remote bytes are
    /// unchanged; only rebuild when the server actually returned new content.
    fn refresh_remote(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let DocumentSource::Remote {
            connection,
            project_root,
            ..
        } = self.source.clone()
        else {
            return;
        };
        self.load_generation = self.load_generation.wrapping_add(1);
        let generation = self.load_generation;
        let path = self.current_path.clone();
        let remote_path = path.to_string_lossy().into_owned();
        self.remote_refreshing = true;
        self.refresh_warning = None;
        self.error = None;
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    crate::remote_ssh::read_file_content(&connection, &project_root, &remote_path)
                })
                .await;
            let _ = this.update_in(cx, |view: &mut FileViewer, window, cx| {
                if view.current_path != path || view.load_generation != generation {
                    return;
                }
                view.remote_refreshing = false;
                view.validate_remote_source(cx);
                if view.remote_source_invalid {
                    return;
                }
                match outcome {
                    Ok(content) => {
                        view.refresh_warning = None;
                        let editable = view.editor.is_some()
                            && !content.content.is_binary
                            && !content.content.too_large;
                        let unchanged = editable
                            && LineEnding::detect(&content.content.content) == view.line_ending
                            && normalize_to_lf(&content.content.content) == view.saved;
                        if unchanged {
                            view.remote_baseline = content.baseline;
                            view.remote_conflict = None;
                            view.error = None;
                        } else if view.dirty {
                            // The user started typing while the refresh was in
                            // flight. Preserve the draft and surface the same
                            // explicit reload/overwrite decision used by save.
                            view.remote_conflict = Some(content);
                        } else {
                            let save_warning = view.save_warning.clone();
                            view.apply_remote_content(content, window, cx);
                            // A successful refresh resolves only the refresh
                            // warning. A prior committed-save cleanup warning
                            // remains actionable until the next save/reload.
                            view.save_warning = save_warning;
                        }
                    }
                    Err(error) => {
                        match remote_refresh_failure_presentation(
                            view.result.is_some(),
                            view.editor.is_some(),
                        ) {
                            RemoteRefreshFailurePresentation::Warning => {
                                view.refresh_warning = Some(error);
                                view.error = None;
                            }
                            RemoteRefreshFailurePresentation::Fatal => {
                                view.refresh_warning = None;
                                view.error = Some(error);
                                if view.can_take_async_focus(window, cx) {
                                    view.focus.focus(window);
                                }
                            }
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn apply_file_content(
        &mut self,
        res: FileContentResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.line_ending = LineEnding::detect(&res.content);
        let text = normalize_to_lf(&res.content);
        self.saved = text.clone();
        self.disk = text.clone();
        self.dirty = false;
        self.ext_changed = false;
        self.preview_draft = None;
        self.save_error = None;
        self.save_warning = None;
        self.refresh_warning = None;

        if can_edit(self.is_img(), Some(&res)) {
            let name = self.file_name();
            let lang = language_for(&name);
            let wrap = should_wrap(&name);
            let editor = cx.new(|cx| {
                InputState::new(window, cx)
                    .code_editor(lang)
                    .line_number(true)
                    .soft_wrap(wrap)
                    .default_value(text.clone())
            });
            // 每次编辑都要重算脏态(原版 `onDocChange` → `setDirty(doc !== savedRef)`)
            let sub = cx.subscribe(&editor, |this: &mut FileViewer, editor, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = editor.read(cx).value().to_string();
                    this.dirty = value != this.saved;
                    cx.notify();
                }
            });
            self._editor_sub = Some(sub);
            self.editor = Some(editor.clone());

            // 命中行定位(全局搜索点进来那条路)。`highlight_line` 是 **1-based**,
            // `Position` 是 0-based;越界直接不动(原版
            // `if (highlightLine > view.state.doc.lines) return`)。
            // `set_cursor_position` 内部 `move_to` → `scroll_to`,滚动是白送的。
            if let Some(line) = highlight_target(self.highlight_line, &text) {
                editor.update(cx, |state, cx| {
                    state.set_cursor_position(Position::new(line - 1, 0), window, cx);
                });
            }
        } else {
            // A remote file can change from editable text to binary/oversized
            // between activations. Drop the old hidden editor so `draft()` and a
            // later refresh cannot reuse stale text behind the fallback view.
            self.editor = None;
            self._editor_sub = None;
        }
        self.result = Some(res);
        // 原版编辑器每次都是带 `autoFocus` 重新挂载的(`preview` 态下才不抢焦点),
        // 这里在内容落位之后统一把焦点摆回该在的地方。工作区允许多个并发加载
        // 的文档，后台页签的迟到结果不得抢走当前页的键盘焦点。
        if self.can_take_async_focus(window, cx) {
            self.focus_content(window, cx);
        }
        cx.notify();
    }

    /// 已经打开的搜索结果再次被点到时，只移动光标，不重建文档或撤销栈。
    pub fn reveal_line(
        &mut self,
        highlight_line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.highlight_line = highlight_line;
        if highlight_line.is_some() && self.has_preview_toggle() && self.preview {
            self.preview = false;
            cx.notify();
        }
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let text = editor.read(cx).value().to_string();
        if let Some(line) = highlight_target(highlight_line, &text) {
            editor.update(cx, |state, cx| {
                state.set_cursor_position(Position::new(line - 1, 0), window, cx);
            });
        }
    }

    /// 检查远程页签的连接快照是否仍对应当前项目配置。
    pub fn validate_remote_source(&mut self, cx: &mut Context<Self>) {
        let DocumentSource::Remote {
            project_id,
            connection,
            project_root,
            ..
        } = &self.source
        else {
            return;
        };
        let (current_root, current) = {
            let store = crate::store::AppStore::global(cx);
            let store = store.read(cx);
            (
                store
                    .project(project_id)
                    .map(|project| project.path.clone()),
                store.remote_connection_of(project_id),
            )
        };
        let invalid = current_root.as_deref() != Some(project_root.as_str())
            || current.as_ref().is_none_or(|current| {
                current.id != connection.id
                    || crate::remote_ssh::connection_fingerprint(current)
                        != crate::remote_ssh::connection_fingerprint(connection)
            });
        if self.remote_source_invalid != invalid {
            self.remote_source_invalid = invalid;
            cx.notify();
        }
    }

    /// 页签重新激活时，干净的远程文档后台重读一次；内容未变时保留编辑器实体，
    /// 脏草稿只做连接身份检查，外部变化继续由保存前基线比较兜底。
    pub fn on_activated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.validate_remote_source(cx);
        if self.source.is_remote()
            && !self.is_img()
            && !self.remote_source_invalid
            && !self.loading
            && !self.remote_refreshing
            && !self.saving
            && !self.dirty
        {
            // Project switches reach this path from WorkbenchArea's deferred focus
            // hand-off. Keep focus on the newly visible document while the remote
            // refresh is in flight; otherwise the hidden editor from the previous
            // project can continue receiving keystrokes until SFTP completes.
            if self.can_take_async_focus(window, cx) {
                self.focus_content(window, cx);
            }
            self.refresh_remote(window, cx);
        } else if self.can_take_async_focus(window, cx) {
            self.focus_content(window, cx);
        }
    }

    /// 当前草稿(编辑器全文,`\n` 行尾)。没有编辑器时就是磁盘内容。
    fn draft(&self, cx: &App) -> String {
        match &self.editor {
            Some(editor) => editor.read(cx).value().to_string(),
            None => self.saved.clone(),
        }
    }

    // ── 监听外部修改 ──────────────────────────────────────

    /// 换文件时把监听挪到新文件的**父目录**上(notify 是目录级监听)。
    /// `FsWatcher` 内部有引用计数,与文件树同时监听同一目录是安全的。
    fn rewatch(&mut self) {
        if self.source.is_remote() {
            if let Some(old) = self.watched.take() {
                self.watcher.unwatch(&old);
            }
            return;
        }
        let dir = self.current_path.parent().map(|p| p.to_path_buf());
        if self.watched == dir {
            return;
        }
        if let Some(old) = self.watched.take() {
            self.watcher.unwatch(&old);
        }
        if let Some(dir) = dir {
            let project = self.project_root.to_string_lossy().to_string();
            if self.watcher.watch(&dir, &project).is_ok() {
                self.watched = Some(dir);
            }
        }
    }

    /// 逐条对照 `FileViewerModal.tsx:275-283`。
    fn on_fs_change(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.source.is_remote() || self.is_img() || self.result.is_none() {
            return;
        }
        if !same_path(&path.to_string_lossy(), &self.path_str()) {
            return;
        }
        // 自己 write 落盘触发的回声,不算「外部」修改
        if self
            .last_save_at
            .is_some_and(|at| at.elapsed() < ECHO_WINDOW)
        {
            return;
        }
        if self.draft(cx) != self.saved || self.saving {
            // 脏或正在保存:先挂提示条，不能在旧写入尚未收口时重建编辑器。
            self.ext_changed = true;
            cx.notify();
        } else {
            // 干净:静默重载跟上磁盘
            self.reload(window, cx);
        }
    }

    // ── 保存 ──────────────────────────────────────────────

    /// `FileViewerModal.tsx:251-272`。干净或在保存中时**静默返回** ——
    /// Ctrl+S 是肌肉记忆,不该弹任何东西。
    fn save(&mut self, cx: &mut Context<Self>) {
        self.save_with_mode(false, cx);
    }

    fn save_with_mode(&mut self, force: bool, cx: &mut Context<Self>) {
        let text = self.draft(cx);
        if self.saving || text == self.saved {
            return;
        }
        if self.remote_refreshing {
            // Saving performs its own fresh baseline validation. Invalidate the
            // older activation refresh so its late result cannot replace a draft
            // or conflict state owned by this save.
            self.load_generation = self.load_generation.wrapping_add(1);
            self.remote_refreshing = false;
        }
        self.validate_remote_source(cx);
        if self.remote_source_invalid {
            return;
        }
        self.saving = true;
        self.save_error = None;
        self.save_warning = None;
        self.remote_conflict = None;
        cx.notify();

        let path = self.current_path.clone();
        let generation = self.load_generation;
        // 写回磁盘前把行尾还原(见模块注释)
        let on_disk = restore_line_ending(&text, self.line_ending);
        match self.source.clone() {
            DocumentSource::Local { project_root, .. } => {
                cx.spawn(async move |this, cx| {
                    let probe = (project_root, path.clone(), on_disk);
                    let outcome = cx
                        .background_executor()
                        .spawn(async move {
                            mt_project::fs::write_file_content(&probe.0, &probe.1, &probe.2)
                        })
                        .await;
                    let _ = this.update(cx, |view: &mut FileViewer, cx| {
                        if view.current_path != path || view.load_generation != generation {
                            return;
                        }
                        view.saving = false;
                        match outcome {
                            Ok(()) => view.finish_save(text.clone(), None, None, cx),
                            Err(err) => view.save_error = Some(format!("{err:#}")),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            DocumentSource::Remote {
                project_id,
                project_root,
                ..
            } => {
                let Some(baseline) = self.remote_baseline.clone() else {
                    self.saving = false;
                    self.save_error = Some(t("fileViewer", "remoteReadOnly").to_string());
                    cx.notify();
                    return;
                };
                let connection = {
                    let store_entity = crate::store::AppStore::global(cx);
                    let store = store_entity.read(cx);
                    store.remote_connection_of(&project_id)
                };
                let Some(connection) = connection else {
                    self.saving = false;
                    self.remote_source_invalid = true;
                    cx.notify();
                    return;
                };
                let remote_path = path.to_string_lossy().into_owned();
                cx.spawn(async move |this, cx| {
                    let outcome = cx
                        .background_executor()
                        .spawn(async move {
                            crate::remote_ssh::save_file_content(
                                &connection,
                                &project_root,
                                &remote_path,
                                &on_disk,
                                &baseline,
                                force,
                            )
                        })
                        .await;
                    let _ = this.update(cx, |view: &mut FileViewer, cx| {
                        if view.current_path != path || view.load_generation != generation {
                            return;
                        }
                        view.saving = false;
                        view.validate_remote_source(cx);
                        if view.remote_source_invalid {
                            return;
                        }
                        let save_succeeded = matches!(
                            &outcome,
                            Ok(crate::remote_ssh::RemoteFileSaveResult::Saved { .. })
                        );
                        view.refresh_warning = refresh_warning_after_remote_save(
                            view.refresh_warning.take(),
                            save_succeeded,
                        );
                        match outcome {
                            Ok(crate::remote_ssh::RemoteFileSaveResult::Saved {
                                baseline,
                                warning,
                            }) => {
                                view.finish_save(text.clone(), Some(baseline), warning, cx);
                            }
                            Ok(crate::remote_ssh::RemoteFileSaveResult::ExternalChange {
                                current,
                            }) => {
                                view.remote_conflict = Some(current);
                            }
                            Err(err) => view.save_error = Some(err),
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    fn finish_save(
        &mut self,
        text: String,
        remote_baseline: Option<crate::remote_ssh::RemoteFileBaseline>,
        warning: Option<String>,
        cx: &App,
    ) {
        self.saved = text.clone();
        self.disk = text.clone();
        self.last_save_at = Some(Instant::now());
        self.remote_baseline = remote_baseline.or_else(|| self.remote_baseline.clone());
        self.remote_conflict = None;
        self.save_warning = warning;
        // 保存期间用户可能又敲了字:按**最新**草稿重新比对。
        self.dirty = self.draft(cx) != text;
        self.ext_changed = false;
    }

    // ── 关闭 ──────────────────────────────────────────────

    /// 工作区页签关闭入口。Workbench 会回读当前 `dirty` 状态并统一处理确认框。
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Workbench 的关闭检查会回读当前 FileViewer 的 dirty 状态。当前按键
        // listener 仍持有本实体的 update 租约，直接回调会 double-lease。
        let source = self.source.clone();
        window.defer(cx, move |window, cx| {
            crate::workbench_area::close_document_source(source, window, cx);
        });
    }

    /// 打开 / 换文件后把焦点放到该放的地方:能编辑就进编辑器,
    /// 否则留在容器上(Ctrl+S / Esc 挂在容器的 `on_key_down` 上,
    /// 焦点不在这条链上就收不到键)。
    fn can_take_async_focus(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        crate::workbench_area::is_document_active(&self.source, cx)
            && !window.has_active_dialog(cx)
            && crate::overlay::allows(crate::overlay::Yield::ToOverlay)
    }

    pub fn focus_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.editor {
            Some(editor) if !(self.has_preview_toggle() && self.preview) => {
                editor.update(cx, |state, cx| state.focus(window, cx));
            }
            _ => self.focus.focus(window),
        }
    }

    /// Route the workspace Ctrl/Cmd+F action into this document. Preview pages
    /// first reveal source, then dispatch the editor's native search action once
    /// the Input node exists in the next rendered dispatch tree.
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loading || self.error.is_some() || !can_edit(self.is_img(), self.result.as_ref()) {
            return;
        }
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        let was_preview = self.has_preview_toggle() && self.preview;
        if was_preview {
            self.preview = false;
            cx.notify();
        }
        editor.update(cx, |state, cx| state.focus(window, cx));
        let focus = editor.read(cx).focus_handle(cx);
        if was_preview {
            let source = self.source.clone();
            window.on_next_frame(move |window, cx| {
                if crate::workbench_area::is_document_active(&source, cx)
                    && !window.has_active_dialog(cx)
                    && crate::overlay::allows(crate::overlay::Yield::ToOverlay)
                {
                    focus.dispatch_action(&Search, window, cx);
                }
            });
        } else {
            focus.dispatch_action(&Search, window, cx);
        }
    }

    /// 「用浏览器打开」。走**协议**关联而不是文件关联 —— `.html` 的默认程序常被
    /// 设成编辑器(用户实测 notepad--),那样点一下只是再开一个编辑器,拿不到
    /// 这个按钮真正想要的东西(见 `mt_project::editor::open_path_in_browser`)。
    fn open_in_browser(&self, cx: &mut App) {
        if self.source.is_remote() {
            return;
        }
        let path = self.current_path.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = mt_project::editor::open_path_in_browser(&path) {
                    eprintln!("[file-viewer] 浏览器打开失败: {err:#}");
                }
            })
            .detach();
    }

    fn open_with_default_app(&self, cx: &mut App) {
        if self.source.is_remote() {
            return;
        }
        let path = self.current_path.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = mt_project::editor::open_path_with_default_app(&path) {
                    eprintln!("[file-viewer] 默认程序打开失败: {err:#}");
                }
            })
            .detach();
    }

    fn download_remote_file(&self, window: &mut Window, cx: &mut App) {
        let DocumentSource::Remote {
            project_id,
            connection,
            project_root,
            ..
        } = &self.source
        else {
            return;
        };
        crate::file_tree::download_remote_file(
            project_id,
            project_root,
            &connection.id,
            crate::remote_ssh::connection_fingerprint(connection),
            self.current_path.clone(),
            window,
            cx,
        );
    }

    // ── 渲染 ──────────────────────────────────────────────

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.file_name();
        let path = self.path_str();
        let is_html = !self.source.is_remote() && is_html_file(&path);
        let can_edit = !self.remote_source_invalid && can_edit(self.is_img(), self.result.as_ref());
        let dirty = self.dirty;
        let saving = self.saving;

        div()
            .flex()
            .items_center()
            .justify_between()
            .px(px(16.0))
            .py(px(12.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .flex_none()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .min_w(px(0.0))
                    .child(FileIcon::new(&name, false, false).size(px(16.0)))
                    .child(
                        div()
                            .flex_none()
                            .text_size(ui::font_px(15.0))
                            .text_color(ui::accent())
                            .child(name),
                    )
                    // 脏点:6px 实心 accent,悬停是「未保存」
                    .when(dirty, |el| {
                        el.child(
                            div()
                                .id("file-viewer-dirty")
                                .w(px(6.0))
                                .h(px(6.0))
                                .flex_none()
                                .rounded_full()
                                .bg(ui::accent())
                                .tooltip(|window, cx| {
                                    Tooltip::new(t("fileViewer", "unsaved")).build(window, cx)
                                }),
                        )
                    })
                    .child(
                        div()
                            .min_w(px(0.0))
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_muted())
                            .truncate()
                            .child(path),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .flex_none()
                    // 保存按钮只在能编辑时画。脏时实心 accent、干净时描边灰
                    .when(can_edit, |el| {
                        let label = if saving {
                            t("fileViewer", "saving")
                        } else {
                            t("fileViewer", "save")
                        };
                        el.child(if dirty && !saving {
                            ui::primary_button("file-viewer-save", label)
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.save(cx)
                                }))
                                .into_any_element()
                        } else {
                            // 干净 / 保存中 = 不可点(原版 `disabled={!dirty || saving}`)
                            div()
                                .px(px(10.0))
                                .py(px(4.0))
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(ui::border_default())
                                .text_size(ui::font_px(12.0))
                                .text_color(ui::text_muted())
                                .child(label)
                                .into_any_element()
                        })
                    })
                    // HTML 常驻「用浏览器打开」:内嵌的那份是无 CSS / 无 JS 的
                    // 简版渲染(见 render_html),真效果只有浏览器给得了
                    .when(is_html, |el| {
                        el.child(
                            div()
                                .id("file-viewer-open-browser")
                                .px(px(10.0))
                                .py(px(4.0))
                                .rounded(px(4.0))
                                .border_1()
                                .border_color(ui::border_default())
                                .text_size(ui::font_px(12.0))
                                .text_color(ui::text_muted())
                                .cursor_pointer()
                                .hover(|el| {
                                    el.text_color(ui::text_primary())
                                        .border_color(ui::border_strong())
                                })
                                .child(t("fileViewer", "openInBrowser"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                    this.open_in_browser(cx)
                                })),
                        )
                    })
                    .when(self.has_preview_toggle(), |el| {
                        el.child(self.render_preview_toggle(cx))
                    }),
            )
    }

    /// 「预览 / 源码」段控件(`FileViewerModal.tsx:355-374`)。
    fn render_preview_toggle(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let preview = self.preview;
        let seg = |id: &'static str, label: String, active: bool| {
            div()
                .id(id)
                .px(px(10.0))
                .py(px(4.0))
                .text_size(ui::font_px(12.0))
                .cursor_pointer()
                .when(active, |el| el.bg(ui::accent()).text_color(ui::bg_base()))
                .when(!active, |el| el.text_color(ui::text_muted()))
                .child(label)
        };

        div()
            .flex()
            .rounded(px(4.0))
            .border_1()
            .border_color(ui::border_default())
            .overflow_hidden()
            .child(
                seg("file-viewer-preview", t("fileViewer", "preview").to_string(), preview)
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        // 切到预览时拍一份草稿快照:预览渲染的是「正在编辑的内容」,
                        // 不是磁盘旧文;干净时置 None,直接用磁盘内容
                        let draft = this.draft(cx);
                        this.preview_draft = (draft != this.saved).then_some(draft);
                        this.preview = true;
                        cx.notify();
                    })),
            )
            .child(
                seg("file-viewer-source", t("fileViewer", "source").to_string(), !preview)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.preview = false;
                        this.focus_content(window, cx);
                        cx.notify();
                    })),
            )
    }

    /// 顶部状态条：保存错误、本地/远程外部修改和连接身份失效。
    fn render_banners(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .flex_none()
            .when(self.remote_source_invalid, |el| {
                el.child(
                    div()
                        .px(px(16.0))
                        .py(px(6.0))
                        .border_b_1()
                        .border_color(ui::border_subtle())
                        .bg(ui::with_alpha(ui::color_warning(), 0.15))
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::color_warning())
                        .child(t("fileViewer", "remoteConnectionChanged")),
                )
            })
            .when_some(self.save_error.clone(), |el, err| {
                el.child(
                    div()
                        .px(px(16.0))
                        .py(px(6.0))
                        .border_b_1()
                        .border_color(ui::border_subtle())
                        .bg(ui::with_alpha(ui::color_error(), 0.15))
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::color_error())
                        .truncate()
                        .child(format!("{}: {}", t("fileViewer", "saveFailed"), err)),
                )
            })
            .when_some(self.save_warning.clone(), |el, warning| {
                el.child(
                    div()
                        .px(px(16.0))
                        .py(px(6.0))
                        .border_b_1()
                        .border_color(ui::border_subtle())
                        .bg(ui::with_alpha(ui::color_warning(), 0.15))
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::color_warning())
                        .truncate()
                        .child(format!("{}: {}", t("fileViewer", "saveWarning"), warning)),
                )
            })
            .when_some(self.refresh_warning.clone(), |el, warning| {
                el.child(
                    div()
                        .px(px(16.0))
                        .py(px(6.0))
                        .border_b_1()
                        .border_color(ui::border_subtle())
                        .bg(ui::with_alpha(ui::color_warning(), 0.15))
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::color_warning())
                        .truncate()
                        .child(format!(
                            "{}: {}",
                            t("fileViewer", "refreshWarning"),
                            warning
                        )),
                )
            })
            .when(
                self.remote_conflict.is_some() && !self.remote_source_invalid,
                |el| {
                    el.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.0))
                            .px(px(16.0))
                            .py(px(6.0))
                            .border_b_1()
                            .border_color(ui::border_subtle())
                            .bg(ui::accent_subtle())
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::color_warning())
                            .child(t("fileViewer", "remoteExternallyChanged"))
                            .child(
                                div()
                                    .id("file-viewer-remote-reload")
                                    .cursor_pointer()
                                    .hover(|el| el.text_color(ui::text_primary()))
                                    .child(t("fileViewer", "reloadDiscard"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        let Some(current) = this.remote_conflict.take() else {
                                            return;
                                        };
                                        this.apply_remote_content(current, window, cx);
                                    })),
                            )
                            .child(
                                div()
                                    .id("file-viewer-remote-force-save")
                                    .cursor_pointer()
                                    .hover(|el| el.text_color(ui::text_primary()))
                                    .child(t("fileViewer", "forceSave"))
                                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                                        this.save_with_mode(true, cx);
                                    })),
                            ),
                    )
                },
            )
            .when(self.ext_changed, |el| {
                el.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .px(px(16.0))
                        .py(px(6.0))
                        .border_b_1()
                        .border_color(ui::border_subtle())
                        .bg(ui::accent_subtle())
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::color_warning())
                        .child(t("fileViewer", "externallyChanged"))
                        .child(
                            div()
                                .id("file-viewer-reload")
                                .when(!self.saving, |el| {
                                    el.cursor_pointer()
                                        .hover(|el| el.text_color(ui::text_primary()))
                                })
                                .when(self.saving, |el| el.opacity(0.5))
                                .child(t("fileViewer", "reloadDiscard"))
                                .when(!self.saving, |el| {
                                    el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.reload(window, cx);
                                    }))
                                }),
                        ),
                )
            })
    }

    /// 居中一行字 + 一个「使用默认工具打开」按钮(二进制 / 过大 / 图片解不出来)。
    fn render_fallback(
        &self,
        id: &'static str,
        message: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let remote = self.source.is_remote();
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(16.0))
            .text_size(ui::font_px(13.0))
            .text_color(ui::text_muted())
            .child(message)
            .when(!remote, |el| {
                el.child(
                    ui::primary_button(id, t("fileViewer", "openWithDefaultApp")).on_click(
                        cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.open_with_default_app(cx)
                        }),
                    ),
                )
            })
            .when(remote, |el| el.child(t("fileViewer", "remoteDownloadHint")))
            .when(remote && !self.remote_source_invalid, |el| {
                el.child(
                    ui::primary_button(id, t("fileTree", "menu.download")).on_click(cx.listener(
                        |this, _: &ClickEvent, window, cx| this.download_remote_file(window, cx),
                    )),
                )
            })
    }

    fn render_center(&self, text: String, color: gpui::Hsla) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_size(ui::font_px(13.0))
            .text_color(color)
            .child(text)
    }

    /// 图片分支。**位图与 svg 都走 `gpui::img(Resource::Path)`** ——
    /// 那条路里 gpui 对 svg 做了 `swap_rgba_pa_to_bgra`(`elements/img.rs:698-703`),
    /// 颜色与预乘 alpha 都是对的;`mt_ui::icons::vector` 注释里记的红蓝互换
    /// 是**另一条路**(`Image::from_bytes(ImageFormat::Svg, …)` 走 `platform.rs`
    /// 的 `to_image_data`,那里确实漏了交换)。
    ///
    /// 解不出来的格式(`image` crate 默认 feature 不含 avif 解码)不留白屏:
    /// 走 [`Self::render_fallback`] 给一个「使用默认工具打开」。
    fn render_image(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let resource = Resource::Path(Arc::from(self.current_path.as_path()));
        match window.use_asset::<ImageAssetLoader>(&resource, cx) {
            None => self
                .render_center(t("fileViewer", "loading").to_string(), ui::text_muted())
                .into_any_element(),
            Some(Err(_)) => self
                .render_fallback(
                    "file-viewer-image-fallback",
                    t("fileViewer", "binaryNotSupported").to_string(),
                    cx,
                )
                .into_any_element(),
            Some(Ok(_)) => div()
                .size_full()
                .p(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                // Img 的 object_fit 默认就是 Contain,与原版 `object-contain` 同义
                .child(img(self.current_path.clone()).size_full())
                .into_any_element(),
        }
    }

    /// 自绘一行 md 图片(纯图片段落由 [`split_top_level_image_paragraph`] 拆出)。
    ///
    /// TextView 那条路把图片目标一律当**网络 URI**(见本模块「markdown 分段」
    /// 一节),于是 README 里 `![主界面](docs/screenshots/main.png)` 这种相对路径
    /// 在预览里什么都不出 —— 原版是 `convertFileSrc(fileDir + '/' + src)`。
    /// 这里按当前文件所在目录解析成 `Resource::Path` 自己画。
    ///
    /// 远程图片先画不触网的占位；用户明确点击后才把 `Resource::Uri` 交给
    /// [`PreviewHttpClient`]。本地 Markdown 维持原来的自动加载行为。
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
            // 本地目标要走「页内跳转」,而那条路整条不做(见模块注释偏差 1)
            let el = match image.link.as_deref().map(str::trim) {
                Some(link)
                    if link.starts_with("http://") || link.starts_with("https://") =>
                {
                    let url = link.to_string();
                    let tip = link.to_string();
                    div()
                        .id(gpui::SharedString::from(format!(
                            "file-viewer-md-img-link-{seg_ix}-{ix}"
                        )))
                        .max_w_full()
                        .min_w_0()
                        .cursor_pointer()
                        .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
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
        let resource = Resource::Path(Arc::from(path));
        let hint = path.to_string_lossy().to_string();
        match window.use_asset::<ImageAssetLoader>(&resource, cx) {
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
    /// 是 URI —— 字节由 [`PreviewHttpClient`] 拉回来。
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
        let uri = gpui::SharedUri::from(url.to_string());
        let resource = Resource::Uri(uri.clone());
        match window.use_asset::<ImageAssetLoader>(&resource, cx) {
            None | Some(Err(_)) => md_image_placeholder(
                id,
                label,
                Some(url.to_string()),
                Some(url.to_string()),
            ),
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
    fn preview_source(&self) -> &str {
        self.preview_draft.as_deref().unwrap_or(&self.disk)
    }

    /// 正文分块(带缓存,见 [`MdCache`])。源码或所在目录变了才重切。
    fn md_blocks(
        &self,
        source: &str,
        base_dir: &Path,
        local_resources: bool,
    ) -> Rc<Vec<(f32, MdBlock)>> {
        // 先把命中与否算完再撒手,别让 borrow 活到 borrow_mut 那一行
        let hit = self.md_cache.borrow().as_ref().and_then(|c| {
            (c.source == source && c.base_dir == base_dir && c.local_resources == local_resources)
                .then(|| c.blocks.clone())
        });
        if let Some(blocks) = hit {
            return blocks;
        }

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
                };
                (mt, block)
            })
            .collect();

        let blocks = Rc::new(blocks);
        *self.md_cache.borrow_mut() = Some(MdCache {
            source: source.to_string(),
            base_dir: base_dir.to_path_buf(),
            local_resources,
            blocks: blocks.clone(),
        });
        blocks
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
            let text = code_block.text.get_or_insert_default();
            text.font_size = Some(ui::font_px(11.9).into());
            text.line_height = Some(gpui::relative(1.6).into());
        }
        TextViewStyle {
            highlight_theme: cx.theme().highlight_theme.clone(),
            is_dark: cx.theme().mode.is_dark(),
            heading_base_font_size: ui::font_px(14.0),
            // 段间距曾按原版 p margin 0.8em 压到 0.7rem,用户体感偏密 ——
            // 回到组件默认 1rem(16px,也接近原版 ul 的浏览器默认 margin 档)
            paragraph_gap: gpui::rems(1.0),
            code_block,
            ..Default::default()
        }
        .heading_font_size(|level, base| match level {
            1 => base * 1.8,
            2 => base * 1.4,
            3 => base * 1.15,
            _ => base,
        })
    }

    /// Markdown 预览。样式对照 `src/styles.css:813-943` 的 `.md-preview`:
    /// 容器 `p-6 max-w-[860px] mx-auto`、段间距 1 rem、正文 1.08rem/1.7。
    ///
    /// 代码块高亮是**改善**(原版 `.md-preview pre code` 只设颜色不做高亮),
    /// 且与编辑器同一份 `highlight_theme`,两处颜色一致。
    fn render_markdown(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let base_dir = self.preview_base_dir();
        let style = self.preview_text_style(cx);
        // 表格与图片拆出来自绘(组件表格单行截断、图片只认网络 URI,见
        // split_md_blocks 一节的说明),其余段落照走 TextView;段落 id 按段序编,
        // 文档不变即稳定。分块结果跨帧缓存(见 MdCache)——「滚一格重画一遍」
        // 这条路上,每帧重切 40 KB 正文是白烧。
        let blocks = self.md_blocks(self.preview_source(), &base_dir, !self.source.is_remote());
        div()
            .id("file-viewer-md")
            .size_full()
            .overflow_y_scroll()
            .p(px(24.0))
            .text_size(ui::font_px(14.0))
            // 原版 .md-preview 是 1.7;数值对齐后用户仍觉得密(体感口径),
            // 放宽到 1.85 —— 表格格子行高同源跟随
            .line_height(gpui::relative(1.85))
            .child(
                div()
                    .w_full()
                    .max_w(px(MARKDOWN_CONTENT_MAX_WIDTH))
                    .min_w_0()
                    .mx_auto()
                    .children(
                        blocks
                            .iter()
                            .enumerate()
                            .map(|(ix, (mt, block))| {
                                // 块间距按原版纵向节奏由这里统一给(em 基准,随
                                // uiFontSize 缩放),TextView 内部的 paragraph_gap
                                // 在非虚拟化路径上是坏的(见 split_md_blocks 注释)
                                let content = match block {
                                    MdBlock::Text(text) => TextView::markdown(
                                        gpui::SharedString::from(format!(
                                            "file-viewer-md-body-{ix}"
                                        )),
                                        text.clone(),
                                        window,
                                        cx,
                                    )
                                    .style(style.clone())
                                    .selectable(true)
                                    .into_any_element(),
                                    MdBlock::Table(table) => {
                                        render_md_table(ix, table, &style, window, cx)
                                    }
                                    MdBlock::Images(images) => self.render_md_images(
                                        ix,
                                        images,
                                        MARKDOWN_CONTENT_MAX_WIDTH,
                                        window,
                                        cx,
                                    ),
                                };
                                div()
                                    .w_full()
                                    .max_w_full()
                                    .min_w_0()
                                    .when(*mt > 0.0, |el| el.mt(ui::font_px(*mt)))
                                    .child(content)
                                    .into_any_element()
                            })
                            .collect::<Vec<_>>(),
                    ),
            )
            .into_any_element()
    }

    /// Trusted local HTML preview. **富文本简版渲染,不是浏览器** —— GPUI 侧没有 iframe 等价物,
    /// `TextView::html` 与 markdown 那支是同一个渲染器:标题 / 段落 / 列表 /
    /// 表格 / 图片 / 链接认得,CSS 与脚本一概不跑,带样式的页面会走样。
    ///
    /// 这正是当初「只留源码态」的理由(模块注释偏差 2)。现在改为提供,配套两条:
    /// 顶上一句说明写清楚它是简版,工具栏常驻「用浏览器打开」给真效果的出口。
    /// 图片与其它本地资源靠 [`rewrite_html_urls`] 转 `file://`(原版是
    /// `convertFileSrc`),由 [`PreviewHttpClient`] 读盘。
    fn render_html(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        debug_assert!(!self.source.is_remote());
        let source = rewrite_html_urls(self.preview_source(), &self.preview_base_dir());
        let style = self.preview_text_style(cx);
        div()
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
                        TextView::html("file-viewer-html-body", source, window, cx)
                            .style(style)
                            .selectable(true),
                    ),
            )
            .into_any_element()
    }

    fn render_content(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        match branch_of(
            self.is_img(),
            self.loading,
            self.error.is_some(),
            self.result.as_ref(),
        ) {
            Branch::Image if self.source.is_remote() => self
                .render_fallback(
                    "file-viewer-remote-image",
                    t("fileViewer", "binaryNotSupported").to_string(),
                    cx,
                )
                .into_any_element(),
            Branch::Image => self.render_image(window, cx),
            Branch::Loading => self
                .render_center(t("fileViewer", "loading").to_string(), ui::text_muted())
                .into_any_element(),
            Branch::Error => self
                .render_center(
                    self.error.clone().unwrap_or_default(),
                    ui::color_error(),
                )
                .into_any_element(),
            Branch::Binary => self
                .render_fallback(
                    "file-viewer-binary",
                    t("fileViewer", "binaryNotSupported").to_string(),
                    cx,
                )
                .into_any_element(),
            Branch::TooLarge => self
                .render_fallback(
                    "file-viewer-too-large",
                    t("fileViewer", "tooLarge").to_string(),
                    cx,
                )
                .into_any_element(),
            Branch::Editor => {
                if self.has_preview_toggle() && self.preview {
                    return if is_markdown_file(&self.path_str()) {
                        self.render_markdown(window, cx)
                    } else {
                        self.render_html(window, cx)
                    };
                }
                match &self.editor {
                    Some(editor) => {
                        // 编辑器排版对齐原版 `CodeEditor.tsx:109-129`:固定 13px
                        // (字面量,**不随 uiFontSize 缩放** —— 原版就是 '13px' 而非
                        // rem)、行高 1.6、字族 `--app-font-mono`。原版的 mono 链是
                        // JetBrains Mono → Cascadia Code → Consolas;gpui 字族单值,
                        // 主族取 Win11 自带的 Cascadia Code,链尾走 font_fallbacks
                        // (含 CJK/emoji 兜底,文件里的中文注释靠它)。用户配置过
                        // uiFontFamily 时原版把 `--app-font-mono` 一并覆盖
                        // (fontManager.ts:8-18),这里同样让它优先。Input 与行号列
                        // 都吃 window.text_style(),包一层即全部生效。
                        let mut wrap = div().size_full();
                        let ts = wrap.text_style().get_or_insert_default();
                        ts.font_family = Some(
                            ui::ui_font_family().unwrap_or_else(|| "Cascadia Code".into()),
                        );
                        ts.font_fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
                            "Cascadia Mono".into(),
                            "Consolas".into(),
                            "JetBrains Mono".into(),
                            "Microsoft YaHei".into(),
                            "Segoe UI Emoji".into(),
                        ]));
                        ts.font_size = Some(px(13.0).into());
                        ts.line_height = Some(gpui::relative(1.6).into());
                        wrap.child(Input::new(editor).h_full().appearance(false).bordered(false))
                            .into_any_element()
                    }
                    None => div().into_any_element(),
                }
            }
        }
    }
}

impl Focusable for FileViewer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for FileViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("file-viewer")
            .track_focus(&self.focus)
            .key_context("FileViewer")
            .size_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            // Ctrl/Cmd+S 与 Ctrl/Cmd+W。挂在容器上而不是绑 action:
            // 绑成全局 action 要动 `main.rs` 的 bindings 表,而这两个键**只在文件页里**
            // 有意义;`on_key_down` 沿焦点链冒泡上来,焦点在编辑器里照样收得到
            // (gpui-component 的 code editor 不吃 Ctrl+S)。
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let ks = &event.keystroke;
                let mods = &ks.modifiers;
                if ks.key == "w" && mods.secondary() && !mods.shift && !mods.alt {
                    cx.stop_propagation();
                    this.request_close(window, cx);
                    return;
                }
                if ks.key == "s" && mods.secondary() && !mods.shift && !mods.alt {
                    cx.stop_propagation();
                    this.save(cx);
                }
            }))
            .child(self.render_toolbar(cx))
            .child(self.render_banners(cx))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_hidden()
                    .bg(ui::bg_base())
                    .child(self.render_content(window, cx)),
            )
    }
}

impl Drop for FileViewer {
    fn drop(&mut self) {
        if let Some(dir) = self.watched.take() {
            self.watcher.unwatch(&dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(content: &str) -> FileContentResult {
        FileContentResult {
            content: content.to_string(),
            is_binary: false,
            too_large: false,
        }
    }

    fn contains_raw_markdown_html(node: &MarkdownNode) -> bool {
        matches!(node, MarkdownNode::Html(_))
            || node
                .children()
                .is_some_and(|children| children.iter().any(contains_raw_markdown_html))
    }

    fn contains_network_loading_markdown_construct(node: &MarkdownNode) -> bool {
        matches!(
            node,
            MarkdownNode::Html(_) | MarkdownNode::Image(_) | MarkdownNode::ImageReference(_)
        ) || node.children().is_some_and(|children| {
            children
                .iter()
                .any(contains_network_loading_markdown_construct)
        })
    }

    fn contains_active_markdown_construct(node: &MarkdownNode) -> bool {
        matches!(
            node,
            MarkdownNode::Html(_)
                | MarkdownNode::Link(_)
                | MarkdownNode::LinkReference(_)
                | MarkdownNode::Image(_)
                | MarkdownNode::ImageReference(_)
        ) || node
            .children()
            .is_some_and(|children| children.iter().any(contains_active_markdown_construct))
    }

    fn visible_backslash_escaped_source(value: &str) -> String {
        let mut chars = value.chars().peekable();
        let mut visible = String::with_capacity(value.len());
        while let Some(ch) = chars.next() {
            if ch == '\\' && chars.peek().is_some_and(|next| next.is_ascii_punctuation()) {
                visible.push(chars.next().expect("peeked punctuation must remain"));
            } else {
                visible.push(ch);
            }
        }
        visible
    }

    #[test]
    fn 文件类型三条判定与原版正则同口径() {
        assert!(is_markdown_file("D:\\a\\README.md"));
        assert!(is_markdown_file("/x/notes.MARKDOWN"), "大小写不敏感");
        assert!(is_markdown_file("a.mkd") && is_markdown_file("a.mdx"));
        assert!(!is_markdown_file("a.mdx.bak"), "只看最后一段扩展名");

        assert!(is_image_file("a.PNG") && is_image_file("a.jpeg") && is_image_file("a.jpg"));
        assert!(is_image_file("a.svg") && is_image_file("a.ico") && is_image_file("a.avif"));
        assert!(is_image_file("a.tif") && is_image_file("a.tiff"));
        assert!(!is_image_file("a.txt"));

        assert!(is_html_file("a.html") && is_html_file("a.HTM"));
        assert!(!is_html_file("a.xhtml"), "原版正则是 /\\.html?$/,xhtml 不算");

        // 折行只给散文类(CodeEditor.tsx:203-206)
        assert!(should_wrap("a.md") && should_wrap("a.txt"));
        assert!(!should_wrap("a.rs") && !should_wrap("a.json"));

        // 没有扩展名一律不是
        assert!(!is_markdown_file("Makefile") && !is_image_file("Makefile"));
    }

    #[test]
    fn 远程_html_只走源码而本地_html_保留预览() {
        assert!(supports_rich_preview(false, "index.html"));
        assert!(!supports_rich_preview(true, "index.html"));
        assert!(supports_rich_preview(false, "README.md"));
        assert!(supports_rich_preview(true, "README.md"));
    }

    #[test]
    fn 远程刷新失败仅在没有已加载内容时进入致命错误页() {
        assert_eq!(
            remote_refresh_failure_presentation(false, false),
            RemoteRefreshFailurePresentation::Fatal
        );
        assert_eq!(
            remote_refresh_failure_presentation(true, false),
            RemoteRefreshFailurePresentation::Warning
        );
        assert_eq!(
            remote_refresh_failure_presentation(false, true),
            RemoteRefreshFailurePresentation::Warning
        );
    }

    #[test]
    fn 远程保存只有成功才清除刷新警告() {
        let warning = Some("refresh failed".to_string());
        assert_eq!(
            refresh_warning_after_remote_save(warning.clone(), false),
            warning
        );
        assert_eq!(
            refresh_warning_after_remote_save(Some("refresh failed".to_string()), true),
            None
        );
    }

    #[test]
    fn 表格分段_基本两列表() {
        let src = "前文\n\n| 文件 | 职责 |\n|---|---|\n| `a.rs` | 说明 A |\n| b.rs | 说明 B |\n\n后文";
        let segs = split_md_blocks(src);
        assert_eq!(segs.len(), 3);
        assert!(matches!(&segs[0], MdSegment::Text(t) if t.contains("前文")));
        let MdSegment::Table(t) = &segs[1] else {
            panic!("第二段应是表格");
        };
        assert_eq!(t.header, vec!["文件", "职责"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0], vec!["`a.rs`", "说明 A"]);
        assert!(matches!(&segs[2], MdSegment::Text(t) if t.contains("后文")));
    }

    #[test]
    fn 表格分段_围栏代码块里的竖线不算表格() {
        let src = "```\n| a | b |\n|---|---|\n```\n正文";
        let segs = split_md_blocks(src);
        assert_eq!(segs.len(), 1, "围栏内的表格样式行不拆:{segs:?}");
    }

    #[test]
    fn markdown_分块尊重围栏标记与长度() {
        let src = concat!(
            "````md\n",
            "```\n",
            "~~~\n",
            "![tracker](https://attacker.example/pixel)\n",
            "| a | b |\n",
            "|---|---|\n",
            "````\n",
            "正文",
        );
        let segs = split_md_blocks(src);
        assert!(
            segs.iter()
                .all(|segment| matches!(segment, MdSegment::Text(_))),
            "围栏内不得拆出图片或表格:{segs:?}"
        );
    }

    #[test]
    fn markdown_分块不会把跨行行内代码识别为图片() {
        let src = concat!(
            "`example\n",
            "![tracker](https://attacker.example/pixel)\n",
            "example`",
        );
        let segs = split_md_blocks(src);
        assert!(
            segs.iter()
                .all(|segment| matches!(segment, MdSegment::Text(_))),
            "跨行行内代码不得拆出图片资源:{segs:?}"
        );
    }

    #[test]
    fn markdown_分块不会拆开列表容器里的围栏代码() {
        for src in [
            concat!(
                "- ````\n",
                "  before\n",
                "  \n",
                "  ![tracker](https://attacker.example/pixel)\n",
                "  | a | b |\n",
                "  |---|---|\n",
                "  ````\n",
            ),
            concat!(
                "1. ~~~\n",
                "   ![tracker](https://attacker.example/pixel)\n",
                "   ~~~\n",
            ),
        ] {
            let segs = split_md_blocks(src);
            assert!(
                segs.iter()
                    .all(|segment| matches!(segment, MdSegment::Text(_))),
                "列表容器里的代码不得拆出资源块:{segs:?}"
            );
        }
    }

    #[test]
    fn markdown_分块不会拆开_raw_html_代码容器() {
        let src = concat!(
            "<pre>\n",
            "![tracker](https://attacker.example/pixel)\n",
            "\n",
            "| a | b |\n",
            "|---|---|\n",
            "</pre>\n",
        );
        let segs = split_md_blocks(src);
        assert!(
            segs.iter()
                .all(|segment| matches!(segment, MdSegment::Text(_))),
            "raw HTML 容器里的文本不得拆出资源块:{segs:?}"
        );
    }

    #[test]
    fn markdown_分块遇到嵌套引用定义时保留整篇作用域() {
        let src = concat!(
            "> [image]: https://example.com/pixel.png\n",
            "\n",
            "![preview][image]\n",
            "\n",
            "| a | b |\n",
            "|---|---|\n",
            "| 1 | 2 |",
        );
        let segs = split_md_blocks(src);
        assert_eq!(segs.len(), 1, "引用定义存在时不得分块:{segs:?}");
        assert!(matches!(&segs[0], MdSegment::Text(text) if text == src));
    }

    #[test]
    fn markdown_分块遇到脚注定义时保留整篇作用域() {
        for src in [
            concat!(
                "正文[^note]\n",
                "\n",
                "[^note]: 脚注正文\n",
                "\n",
                "| a | b |\n",
                "|---|---|\n",
                "| 1 | 2 |",
            ),
            concat!(
                "正文[^note]\n",
                "\n",
                "> [^note]: 引用块里的脚注正文\n",
                "\n",
                "| a | b |\n",
                "|---|---|\n",
                "| 1 | 2 |",
            ),
        ] {
            let segs = split_md_blocks(src);
            assert_eq!(segs.len(), 1, "脚注定义存在时不得分块:{segs:?}");
            assert!(matches!(&segs[0], MdSegment::Text(text) if text == src));
        }
    }

    #[test]
    fn markdown_分块不会把缩进代码识别为表格() {
        let src = concat!(
            "    | x |\n",
            "    | --- |\n",
            "    | ![track](https://example.com/pixel) |",
        );
        let segs = split_md_blocks(src);
        assert!(
            segs.iter()
                .all(|segment| matches!(segment, MdSegment::Text(_))),
            "缩进代码不得拆成表格或图片:{segs:?}"
        );
    }

    #[test]
    fn markdown_分块按制表位识别混合缩进代码() {
        for prefix in ["\t", " \t", "  \t", "   \t"] {
            let image = format!("{prefix}![track](https://example.com/pixel)");
            let image_segments = split_md_blocks(&image);
            assert!(
                image_segments
                    .iter()
                    .all(|segment| matches!(segment, MdSegment::Text(_))),
                "混合缩进图片不得拆出图片段:{prefix:?} {image_segments:?}"
            );

            let table = format!(
                "{prefix}| x |\n{prefix}| --- |\n{prefix}| ![track](https://example.com/pixel) |"
            );
            let table_segments = split_md_blocks(&table);
            assert!(
                table_segments
                    .iter()
                    .all(|segment| matches!(segment, MdSegment::Text(_))),
                "混合缩进表格不得拆出表格段:{prefix:?} {table_segments:?}"
            );
        }
    }

    #[test]
    fn 表格分段_对齐与码段竖线() {
        // 分隔行的 :---: 语法
        let src = "| a | b | c |\n| :--- | :---: | ---: |\n| 1 | 2 | 3 |";
        let MdSegment::Table(t) = &split_md_blocks(src)[0] else {
            panic!()
        };
        assert_eq!(t.aligns, vec![MdAlign::Left, MdAlign::Center, MdAlign::Right]);

        // code span 里的 | 不拆格,\| 是字面竖线
        assert_eq!(split_cells("| `a|b` | c\\|d |"), vec!["`a|b`", "c|d"]);

        // 短行按表头列数补空
        let src = "| a | b |\n|---|---|\n| 仅一格 |";
        let MdSegment::Table(t) = &split_md_blocks(src)[0] else {
            panic!()
        };
        assert_eq!(t.rows[0], vec!["仅一格", ""]);
    }

    #[test]
    fn 分段_空行拆块_围栏内空行不拆_块距节奏() {
        // 空行是块边界:三段文本 + 一个标题 = 四块
        let segs = split_md_blocks("段落一\n\n段落二\n\n### 标题\n\n段落三");
        assert_eq!(segs.len(), 4, "{segs:?}");
        // 块距:首块 0、普通块 11、标题块 20(原版 margin-top 1.4em 的近似)
        assert_eq!(block_top_margin(0, &segs[0]), 0.0);
        assert_eq!(block_top_margin(1, &segs[1]), 11.0);
        assert_eq!(block_top_margin(2, &segs[2]), 20.0);

        // 围栏代码块里的空行不拆块
        let segs = split_md_blocks("```\naaa\n\nbbb\n```");
        assert_eq!(segs.len(), 1, "{segs:?}");

        // `#` 后没空格不算标题;表格块 13(原版 table margin 1em)
        assert_eq!(
            block_top_margin(1, &MdSegment::Text("#hash 不是标题".into())),
            11.0
        );
        let t = MdSegment::Table(MdTable {
            header: vec![],
            aligns: vec![],
            rows: vec![],
        });
        assert_eq!(block_top_margin(3, &t), 13.0);
    }

    #[test]
    fn 表格列宽_短列有底宽_长列封顶() {
        let t = MdTable {
            header: vec!["文件".into(), "职责".into()],
            aligns: vec![MdAlign::Left, MdAlign::Left],
            rows: vec![vec![
                "`process_monitor.rs`".into(),
                "这一格是很长很长的中文说明,足以超过封顶阈值的长度,再加一点点凑数的文字。".into(),
            ]],
        };
        let w = column_weights(&t);
        assert_eq!(w.len(), 2);
        // 第一列 20 字符、第二列封顶 60 → 20/80 = 0.25,短列不至于被压没
        assert!(w[0] > 0.2 && w[0] < 0.3, "第一列权重 {w:?}");
        assert!((w[0] + w[1] - 1.0).abs() < 1e-5);

        // 纯短表:两列都吃底宽,均分
        let t2 = MdTable {
            header: vec!["a".into(), "b".into()],
            aligns: vec![MdAlign::Left, MdAlign::Left],
            rows: vec![],
        };
        let w2 = column_weights(&t2);
        assert!((w2[0] - 0.5).abs() < 1e-5);
    }

    #[test]
    fn 表格格子_纯文字走快路_带标记的交回_textview() {
        // 快路:一句纯文字(表格里的绝大多数)
        assert!(is_plain_cell("已完成"));
        assert!(is_plain_cell("用户登录模块"));
        assert!(is_plain_cell(""), "空格子");
        assert!(is_plain_cell("P0"));
        // `-` 不在行首不是标记;`=` 单行永远成不了 setext 标题
        assert!(is_plain_cell("2026-08-25"));
        assert!(is_plain_cell("a=b"));
        assert!(is_plain_cell("张三 李四"), "单个空格照走快路");

        // 行内标记一律交回
        assert!(!is_plain_cell("`a.rs`"));
        assert!(!is_plain_cell("**必填**"));
        assert!(!is_plain_cell("下划_线"));
        assert!(!is_plain_cell("[文档](a.md)"));
        assert!(!is_plain_cell("![图](a.png)"));
        assert!(!is_plain_cell("~~废弃~~"));
        assert!(!is_plain_cell("<br>"));
        assert!(!is_plain_cell("a&amp;b"));
        assert!(!is_plain_cell("a\\|b"), "转义符");

        // GFM autolink literal:裸 URL / www. / 邮箱会自动成链接
        assert!(!is_plain_cell("https://example.com"));
        assert!(!is_plain_cell("www.example.com"));
        assert!(!is_plain_cell("a@b.com"));

        // 块级标记在行首才算,而格子已 trim,只看开头一处
        assert!(!is_plain_cell("# 标题"));
        assert!(!is_plain_cell("- 列表项"));
        assert!(!is_plain_cell("+ 列表项"));
        assert!(!is_plain_cell("---"), "分隔线");
        assert!(!is_plain_cell("1. 第一步"));
        assert!(!is_plain_cell("2) 第二步"));
        assert!(is_plain_cell("1.5 倍"), "小数不是有序列表");
        assert!(is_plain_cell("2026 年"), "光是数字开头不算");

        // markdown 折叠空白,纯文本不折 —— 有连续空白就交回,免得排版有差
        assert!(!is_plain_cell("a  b"));
        assert!(!is_plain_cell("a\tb"));
    }

    #[test]
    fn 表格格子_真实形状的表大头走快路() {
        // 「文件 | 职责」这类文档表:只有第一列带反引号,其余都是纯文字
        let src = "| 模块 | 负责人 | 状态 | 备注 |\n|---|---|---|---|\n\
                   | `auth.rs` | 张三 | 已完成 | 见设计稿 |\n\
                   | 支付 | 李四 | 进行中 | 依赖第三方 |";
        let MdSegment::Table(t) = &split_md_blocks(src)[0] else {
            panic!("应解析成表格")
        };
        let cells: Vec<&String> = t.header.iter().chain(t.rows.iter().flatten()).collect();
        let fast = cells.iter().filter(|c| is_plain_cell(c)).count();
        assert_eq!(cells.len(), 12);
        assert_eq!(fast, 11, "只有 `auth.rs` 那一格该交回 TextView");
    }

    #[test]
    fn 图片段落_认得五种常见写法() {
        // 单张
        let segments = split_md_blocks("![主界面](docs/screenshots/main.png)");
        let [MdSegment::Images(imgs)] = segments.as_slice() else {
            panic!("单张图片应由 AST 拆出来自绘")
        };
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].url, "docs/screenshots/main.png");
        assert_eq!(imgs[0].alt, "主界面");
        assert!(imgs[0].link.is_none());

        // 带 title
        let segments = split_md_blocks(r#"![图](a.png "标题")"#);
        let [MdSegment::Images(imgs)] = segments.as_slice() else {
            panic!("带标题图片应由 AST 拆出来自绘")
        };
        assert_eq!(imgs[0].url, "a.png");
        assert_eq!(imgs[0].title.as_deref(), Some("标题"));

        // 链接包裹(徽章)
        let segments = split_md_blocks("[![CI](https://img.shields.io/x.svg)](https://ci.example)");
        let [MdSegment::Images(imgs)] = segments.as_slice() else {
            panic!("链接包裹图片应由 AST 拆出来自绘")
        };
        assert_eq!(imgs[0].url, "https://img.shields.io/x.svg");
        assert_eq!(imgs[0].link.as_deref(), Some("https://ci.example"));

        // 一行并排两张
        let segments = split_md_blocks("![a](1.png) ![b](2.png)");
        let [MdSegment::Images(imgs)] = segments.as_slice() else {
            panic!("并排图片应由 AST 拆出来自绘")
        };
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[1].url, "2.png");

        // 尖括号写法(路径里有空格)
        let segments = split_md_blocks("![x](<my shots/a b.png>)");
        let [MdSegment::Images(imgs)] = segments.as_slice() else {
            panic!("尖括号目标图片应由 AST 拆出来自绘")
        };
        assert_eq!(imgs[0].url, "my shots/a b.png");
    }

    #[test]
    fn 图片段落_普通文本与无效_commonmark_不会升级成资源() {
        // 前后有文字 → 交给 TextView(内联图片不自绘)
        for source in [
            "看这张 ![a](1.png)",
            "![a](1.png) 就是主界面",
            "- ![a](1.png)",
            "> ![a](1.png)",
            "    ![a](1.png)",
            "![a]()",
            "[文档](a.md)",
            "![x](https://attacker.example/pixel trailing)",
            "![x](https://attacker.example/pixel \"unclosed)",
        ] {
            let segments = split_md_blocks(source);
            assert!(
                segments
                    .iter()
                    .all(|segment| matches!(segment, MdSegment::Text(_))),
                "普通文本或无效图片语法不得升级成资源:{source:?} {segments:?}"
            );
        }
    }

    #[test]
    fn 纯图片段落自绘_混合段落保留给_textview() {
        let src = "# 标题\n\n上面一句说明\n\n![主界面](docs/main.png)\n\n下面一句";
        let segs = split_md_blocks(src);
        assert_eq!(segs.len(), 4, "{segs:?}");
        let MdSegment::Images(imgs) = &segs[2] else {
            panic!("第三段应是图片:{segs:?}");
        };
        assert_eq!(imgs[0].url, "docs/main.png");
        assert!(matches!(&segs[1], MdSegment::Text(t) if t == "上面一句说明"));
        assert!(matches!(&segs[3], MdSegment::Text(t) if t == "下面一句"));

        let mixed = split_md_blocks("上面一句说明\n![主界面](docs/main.png)\n下面一句");
        assert_eq!(mixed.len(), 1, "混合段落应完整交给 TextView:{mixed:?}");
        assert!(matches!(&mixed[0], MdSegment::Text(_)));

        // 围栏代码块里的图片语法是代码,不拆
        let segs = split_md_blocks("```md\n![a](1.png)\n```");
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert!(matches!(&segs[0], MdSegment::Text(_)));

        let with_definition = split_md_blocks(concat!(
            "![direct](https://example.com/direct.png)\n\n",
            "[docs]: https://example.com/docs\n\n",
            "正文\n",
        ));
        assert!(
            matches!(
                &with_definition[0],
                MdSegment::Images(images) if images[0].url.ends_with("direct.png")
            ),
            "普通定义不得让直链图片失去自绘占位:{with_definition:?}"
        );
        assert_eq!(
            with_definition.len(),
            2,
            "未引用定义不应产生空 TextView 块:{with_definition:?}"
        );
        assert!(matches!(&with_definition[1], MdSegment::Text(text) if text == "正文"));

        let reference = split_md_blocks(concat!(
            "![badge][image]\n\n",
            "[image]: https://example.com/badge.svg\n",
        ));
        assert!(
            matches!(reference.as_slice(), [MdSegment::Text(_)]),
            "引用图片保留整篇定义作用域并在远程 TextView 路径安全降级:{reference:?}"
        );
    }

    #[test]
    fn 远程图片必须先获批准_本地图片保持自动加载() {
        assert!(!markdown_image_can_load(true, false));
        assert!(markdown_image_can_load(true, true));
        assert!(markdown_image_can_load(false, false));
    }

    #[test]
    fn 图片目标_相对路径按当前文件目录解析() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR"));
        // 相对路径 → 落到当前文件所在目录(原版 convertFileSrc(fileDir + '/' + src))
        assert_eq!(
            resolve_image_src("docs/a.png", base),
            MdImageSrc::Local(base.join("docs/a.png"))
        );
        // %20 还原
        assert_eq!(
            resolve_image_src("my%20shots/a.png", base),
            MdImageSrc::Local(base.join("my shots/a.png"))
        );

        // 宿主平台的绝对路径原样
        let absolute = base.join("shots/a.png");
        assert_eq!(
            resolve_image_src(&absolute.to_string_lossy(), base),
            MdImageSrc::Local(absolute)
        );

        #[cfg(windows)]
        {
            // Windows 盘符不能被当成 scheme；file:// 三斜杠会去掉盘符前的 `/`
            assert_eq!(
                resolve_image_src("D:/shots/a.png", base),
                MdImageSrc::Local(PathBuf::from("D:/shots/a.png"))
            );
            assert_eq!(
                resolve_image_src("file:///D:/shots/a.png", base),
                MdImageSrc::Local(PathBuf::from("D:/shots/a.png"))
            );
        }
        // 远程与不认识的 scheme
        assert_eq!(
            resolve_image_src("https://x.dev/a.png", base),
            MdImageSrc::Remote("https://x.dev/a.png".into())
        );
        assert_eq!(
            resolve_image_src("data:image/png;base64,AAA", base),
            MdImageSrc::Unsupported
        );
        assert_eq!(resolve_image_src("  ", base), MdImageSrc::Unsupported);
    }

    #[test]
    fn svg_判定_不被查询串骗到() {
        // 徽章 URL 常带 `?style=`,扩展名只看路径那一截
        assert!(is_svg_target("https://img.shields.io/badge/a-b.svg?style=flat"));
        assert!(is_svg_target("D:\\icons\\a.SVG"));
        assert!(!is_svg_target("https://x.dev/a.png"));
        assert!(!is_svg_target("a/b.svg.png"), "只看最后一段扩展名");
    }

    #[test]
    fn md_内联图片的本地路径改写成_file_url() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs");
        // 列表项里的内联图片(块级图片行走自绘,不经过这条)
        let out = rewrite_md_image_urls("- ![图](shots/a.png) 说明", &base);
        let image_url = to_file_url(&base.join("shots/a.png")).expect("测试基准路径应为绝对路径");
        assert!(out.starts_with(&format!("- ![图]({image_url})")), "{out}");
        // title 保留
        let out = rewrite_md_image_urls(r#"![图](a.png "标题")"#, &base);
        assert!(out.contains(r#""标题""#), "{out}");
        // 远程与 data: 原样
        let remote = "![x](https://x.dev/a.png)";
        assert_eq!(rewrite_md_image_urls(remote, &base), remote);
        let data = "![x](data:image/png;base64,AAA)";
        assert_eq!(rewrite_md_image_urls(data, &base), data);
        // 围栏代码块 / 行内 code 里的图片语法是代码,不许动
        let fenced = "```md\n![a](b.png)\n```";
        assert_eq!(rewrite_md_image_urls(fenced, &base), fenced);
        let inline_code = "写法是 `![a](b.png)` 这样";
        assert_eq!(rewrite_md_image_urls(inline_code, &base), inline_code);
        // 解析器没有确认成 Image 的宽松/残缺写法不得被改写成有效资源。
        for invalid in ["![x](shots/a.png trailing)", "![x](shots/a.png \"unclosed)"] {
            assert_eq!(rewrite_md_image_urls(invalid, &base), invalid);
        }
    }

    #[test]
    fn html_的本地资源改写成_file_url() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("site");
        let image_url = to_file_url(&base.join("img/a.png")).expect("测试基准路径应为绝对路径");
        let out = rewrite_html_urls(r#"<img src="img/a.png" alt="a">"#, &base);
        assert_eq!(out, format!(r#"<img src="{image_url}" alt="a">"#));
        // 单引号 / 大写属性名 / 等号旁的空白都认
        let image_url = to_file_url(&base.join("a.png")).expect("测试基准路径应为绝对路径");
        let out = rewrite_html_urls("<img SRC = 'a.png'>", &base);
        assert_eq!(out, format!("<img SRC = '{image_url}'>"));
        // href / poster 同样处理
        let poster_url = to_file_url(&base.join("p.jpg")).expect("测试基准路径应为绝对路径");
        let out = rewrite_html_urls(r#"<video poster="p.jpg"></video>"#, &base);
        assert!(out.contains(&poster_url), "{out}");

        // 排除清单(原版正则那一串)一律原样
        for keep in [
            r#"<a href="https://x.dev">x</a>"#,
            r#"<img src="data:image/png;base64,AAA">"#,
            // 井号锚点:`"#` 会提前结束 `r#"…"#`,这条必须用 `r##"…"##`
            r##"<a href="#anchor">锚</a>"##,
            r#"<a href="mailto:a@b.c">mail</a>"#,
            r#"<a href="javascript:void(0)">js</a>"#,
            r#"<img src="file:///D:/site/a.png">"#,
        ] {
            assert_eq!(rewrite_html_urls(keep, &base), keep, "不该改:{keep}");
        }
        // `data-src` 不是 src
        let keep = r#"<img data-src="a.png">"#;
        assert_eq!(rewrite_html_urls(keep, &base), keep);

        // 远程清洗器在见过 svg/math 后会保守扫描后续 raw-text，防 HTML5
        // namespace 恢复漏掉活动图片；可信本地 HTML 不得复用这条 fail-closed
        // 策略，否则 textarea 里的示例文本会被误改成 file:// URL。
        let keep = r#"<svg></svg><textarea><img src="literal.png"></textarea>"#;
        assert_eq!(rewrite_html_urls(keep, &base), keep);
    }

    #[test]
    fn 远程富文本禁用自动资源但保留显式网络链接() {
        let markdown = concat!(
            "- ![secret](file:///home/user/secret.png)\n",
            "![tracker](http://127.0.0.1:8080/a.png)\n",
            "`![code](file:///tmp/code.png)`\n",
            "```md\n![fenced](file:///tmp/fenced.png)\n```",
        );
        let sanitized = sanitize_remote_markdown(markdown);
        assert!(sanitized.contains("- secret"), "{sanitized}");
        assert!(sanitized.contains("tracker"), "{sanitized}");
        assert!(!sanitized.contains("![tracker]"), "{sanitized}");
        assert!(!sanitized.contains("file:///home/user/secret.png"));
        assert!(sanitized.contains("`![code](file:///tmp/code.png)`"));
        assert!(sanitized.contains("![fenced](file:///tmp/fenced.png)"));

        let references = sanitize_remote_markdown(concat!(
            "![secret][local]\n",
            "[local]: <file:///home/user/secret.png> \"title\"\n",
            "![web][remote]\n",
            "[remote]: https://example.com/image.png\n",
        ));
        assert!(!references.contains("file:///"), "{references}");
        assert!(
            references.contains("[remote]: https://example.com/image.png"),
            "{references}"
        );
        // Unresolved reference syntax may remain as literal text. The reparsed
        // AST below is the security boundary: no active image/reference node
        // may survive sanitization.
        let references_ast = markdown::to_mdast(&references, &ParseOptions::gfm())
            .expect("sanitized references must remain parseable");
        let mut unsafe_reference_nodes = Vec::new();
        collect_remote_markdown_replacements(&references_ast, &mut unsafe_reference_nodes);
        assert!(unsafe_reference_nodes.is_empty(), "{references}");

        let links = sanitize_remote_markdown(concat!(
            "[local](file:///etc/passwd)\n",
            "[relative](../secret.txt)\n",
            "[web](https://example.com/docs)\n",
            "[<file:///etc/shadow>](file:///tmp/outer)\n",
            "<file:///etc/group>\n",
            "`[code](file:///tmp/code)`\n",
            "``[code](file:///tmp/double)``\n",
            "` unmatched [unsafe](file:///tmp/unmatched)\n",
            "```md\n[code](file:///tmp/fenced)\n```",
        ));
        assert!(!links.contains("file:///etc/passwd"), "{links}");
        assert!(!links.contains("../secret.txt"), "{links}");
        assert!(!links.contains("file:///etc/group"), "{links}");
        assert!(!links.contains("file:///etc/shadow"), "{links}");
        assert!(!links.contains("file:///tmp/outer"), "{links}");
        assert!(!links.contains("file:///tmp/unmatched"), "{links}");
        assert!(links.contains("local\nrelative\n"), "{links}");
        assert!(links.contains("[web](https://example.com/docs)"), "{links}");
        assert!(links.contains("`[code](file:///tmp/code)`"), "{links}");
        assert!(links.contains("``[code](file:///tmp/double)``"), "{links}");
        assert!(links.contains("` unmatched unsafe"), "{links}");
        assert!(links.contains("[code](file:///tmp/fenced)"), "{links}");

        let multiline = sanitize_remote_markdown(concat!(
            "![secret](\nfile:///home/user/secret.png\n)\n",
            "[open](\nfile:///etc/passwd\n)\n",
        ));
        assert!(!multiline.contains("file:///"), "{multiline}");
        assert!(multiline.contains("secret"), "{multiline}");
        assert!(multiline.contains("open"), "{multiline}");

        let decoded_label_injection = sanitize_remote_markdown(concat!(
            "[&#91;open&#93;&#40;file:///etc/passwd&#41;](file:///outer)\n",
            "![&#91;image&#93;&#40;file:///tmp/a.png&#41;](file:///image)\n",
            "[&#91;ref&#93;]: file:///definition\n",
        ));
        assert!(
            !decoded_label_injection.contains("file:///outer"),
            "{decoded_label_injection}"
        );
        assert!(
            !decoded_label_injection.contains("file:///image"),
            "{decoded_label_injection}"
        );
        // 定义不能中断前面的段落；这一行从首次解析起就是普通文本，不会生成链接。
        assert!(
            decoded_label_injection.contains("[&#91;ref&#93;]: file:///definition"),
            "{decoded_label_injection}"
        );
        let ast = markdown::to_mdast(&decoded_label_injection, &ParseOptions::gfm())
            .expect("sanitized markdown must remain parseable");
        let mut unsafe_nodes = Vec::new();
        collect_remote_markdown_replacements(&ast, &mut unsafe_nodes);
        assert!(unsafe_nodes.is_empty(), "{decoded_label_injection}");

        let fence_edges = sanitize_remote_markdown(concat!(
            "    ```\n",
            "[after-indent](file:///tmp/after-indent)\n",
            "```md\n",
            "~~~\n",
            "[inside](file:///tmp/inside)\n",
            "```\n",
            "[outside](file:///tmp/outside)\n",
        ));
        assert!(
            !fence_edges.contains("file:///tmp/after-indent"),
            "{fence_edges}"
        );
        assert!(fence_edges.contains("file:///tmp/inside"), "{fence_edges}");
        assert!(
            !fence_edges.contains("file:///tmp/outside"),
            "{fence_edges}"
        );

        let html = concat!(
            r#"<img src="file:///home/user/secret.png">"#,
            r#"<img src="http://127.0.0.1:8080/a.png">"#,
            r#"<a href="file:///etc/passwd">local</a>"#,
            r#"<a href="https://example.com/docs">web</a>"#,
            r##"<a href="#section">anchor</a>"##,
        );
        let sanitized = sanitize_remote_html_urls(html);
        assert!(!sanitized.contains("file:///"), "{sanitized}");
        assert_eq!(sanitized.matches(r#"src="about:blank""#).count(), 2);
        assert!(sanitized.contains(r##"href="#""##), "{sanitized}");
        assert!(
            sanitized.contains("https://example.com/docs"),
            "{sanitized}"
        );
        assert!(sanitized.contains(r##"href="#section""##), "{sanitized}");

        let unquoted = sanitize_remote_html_urls(concat!(
            r#"<img src=file:///etc/passwd>"#,
            r#"<img/src=file:///etc/group>"#,
            r#"<img alt="x"src=file:///etc/hosts>"#,
            r#"<img src=https://example.com/image.png>"#,
            r#"<a href=../secret.txt>local</a>"#,
        ));
        assert!(!unquoted.contains("file:///"), "{unquoted}");
        assert!(!unquoted.contains("src=https://example.com/image.png"));
        assert!(unquoted.contains("src=about:blank"), "{unquoted}");
        assert!(unquoted.contains("href=#"), "{unquoted}");

        let stray_text = sanitize_remote_html_urls(
            "plain href=\" without a closing quote\n<img src=file:///etc/shadow>",
        );
        assert!(!stray_text.contains("file:///"), "{stray_text}");

        for source in [
            r#"<!-- normal --><img src="https://evil.test/normal.png">"#,
            r#"<!--x--!><img src="https://evil.test/bang.png">"#,
            r#"<!--><img src="https://evil.test/abrupt.png">"#,
            r#"<!--><img src="https://evil.test/abrupt-with-tail.png">-->"#,
            r#"<!---><img src="https://evil.test/short.png">"#,
            r#"</div "><img src="https://evil.test/end-tag.png">"#,
            r#"<script>x</script "><img src="https://evil.test/raw-end-tag.png">"#,
            r#"<svg><script><img src="https://evil.test/foreign.png"></script></svg>"#,
            r#"<svg><p><math></svg><script><img src="https://evil.test/foreign-recovery-a.png"></script>"#,
            r#"<svg></math><p><math></svg><script><img src="https://evil.test/foreign-recovery-b.png"></script>"#,
        ] {
            let html = sanitize_remote_html_urls(source);
            assert!(html.contains(r#"src="about:blank""#), "{html}");
            assert!(!html.contains("src=\"https://evil.test"), "{html}");

            let markdown = sanitize_remote_markdown(source);
            let ast = markdown::to_mdast(&markdown, &ParseOptions::gfm())
                .expect("sanitized Markdown must remain parseable");
            assert!(!contains_raw_markdown_html(&ast), "{markdown}");
            assert!(
                !contains_network_loading_markdown_construct(&ast),
                "{markdown}"
            );
            assert_eq!(visible_backslash_escaped_source(&markdown), source);
        }

        let raw_text = concat!(
            r#"<textarea /><img src="https://example.com/text-example.png"></textarea>"#,
            r#"<img src="https://evil.test/after-textarea.png">"#,
        );
        let scanned = sanitize_remote_html_urls(raw_text);
        assert!(
            scanned.contains("https://example.com/text-example.png"),
            "{scanned}"
        );
        assert!(scanned.contains(r#"src="about:blank""#), "{scanned}");
        assert!(
            !scanned.contains("https://evil.test/after-textarea.png"),
            "{scanned}"
        );

        let markdown = sanitize_remote_markdown(raw_text);
        assert_eq!(visible_backslash_escaped_source(&markdown), raw_text);
        let ast = markdown::to_mdast(&markdown, &ParseOptions::gfm())
            .expect("sanitized Markdown must remain parseable");
        assert!(!contains_raw_markdown_html(&ast), "{markdown}");
        assert!(
            !contains_network_loading_markdown_construct(&ast),
            "{markdown}"
        );
    }

    #[test]
    fn markdown_html_只降级真实_ast_节点并保留代码原文() {
        let source = concat!(
            "`<img src=\"https://example.com/inline.png\">`\n\n",
            "`<Widget src=\"file:///tmp/widget\" />`\n\n",
            "```html\n<a href=\"file:///tmp/example\">example</a>\n```\n\n",
            "```jsx\n<Component href=\"file:///tmp/component\" />\n```\n\n",
            "<pre>\n&lt;img src=\"https://example.com/pre-example.png\"&gt;\n</pre>\n\n",
            "<!-- <img src=\"https://example.com/comment-example.png\"> -->\n\n",
            "<script>const demo = '<img src=\"https://example.com/script-example.png\">';</script>\n\n",
            r#"<img src="https://example.com/active.png">"#,
            "\n",
            r#"<a href="file:///etc/passwd">local</a>"#,
            "\n",
            r#"<a href="https://example.com/docs">web</a>"#,
        );
        let sanitized = sanitize_remote_markdown(source);
        assert!(
            sanitized.contains("`<img src=\"https://example.com/inline.png\">`"),
            "{sanitized}"
        );
        assert!(
            sanitized.contains("`<Widget src=\"file:///tmp/widget\" />`"),
            "{sanitized}"
        );
        assert!(
            sanitized.contains("```html\n<a href=\"file:///tmp/example\">example</a>\n```"),
            "{sanitized}"
        );
        assert!(
            sanitized.contains("```jsx\n<Component href=\"file:///tmp/component\" />\n```"),
            "{sanitized}"
        );
        assert_eq!(visible_backslash_escaped_source(&sanitized), source);

        let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
            .expect("sanitized Markdown must remain parseable");
        assert!(!contains_raw_markdown_html(&ast), "{sanitized}");
        assert!(
            !contains_network_loading_markdown_construct(&ast),
            "{sanitized}"
        );
    }

    #[test]
    fn 审核载荷在远程与会话_markdown中都不能形成活动_html() {
        for payload in [
            r#"<div><select><title></select><img src="https://attacker.example/beacon.png"></title></div>"#,
            r#"<select><plaintext></select><img src="https://attacker.example/b2.png"><a href="file:///C:/Windows/notepad.exe">open</a>"#,
            r#"<template><col><title></template><img src="https://attacker.example/b3.png"></title>"#,
            r#"<div data-example="![beacon](https://attacker.example/b4.png)"></div>"#,
        ] {
            for sanitized in [
                sanitize_remote_markdown(payload),
                sanitize_session_markdown(payload),
            ] {
                assert_eq!(visible_backslash_escaped_source(&sanitized), payload);
                let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
                    .expect("sanitized Markdown must remain parseable");
                assert!(!contains_raw_markdown_html(&ast), "{sanitized}");
                assert!(
                    !contains_network_loading_markdown_construct(&ast),
                    "{sanitized}"
                );
                assert!(!contains_active_markdown_construct(&ast), "{sanitized}");
                let mut unsafe_nodes = Vec::new();
                collect_untrusted_markdown_replacements(&ast, &mut unsafe_nodes);
                assert!(unsafe_nodes.is_empty(), "{sanitized}");
            }
        }
    }

    #[test]
    fn html_block_type_1到5后的缩进活动载荷会清洗到不动点() {
        let html_blocks = [
            "<pre></pre>",
            "<style></style>",
            "<!-- comment -->",
            "<?php ?>",
            "<!DOCTYPE html>",
            "<![CDATA[value]]>",
        ];
        let indented_payloads = [
            "![network](https://attacker.example/image.png)",
            "[local](file:///etc/passwd)",
            "![local](file:///etc/passwd)",
            r#"<img src="https://attacker.example/raw.png">"#,
            r#"<a href="file:///etc/passwd">open</a>"#,
        ];

        for html_block in html_blocks {
            for payload in indented_payloads {
                let source = format!("{html_block}\n    {payload}\n");
                for sanitized in [
                    sanitize_remote_markdown(&source),
                    sanitize_session_markdown(&source),
                ] {
                    assert_ne!(
                        sanitized,
                        markdown_as_indented_code(&source),
                        "正常审核载荷应在轮次上限内收敛:{source}"
                    );
                    let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
                        .expect("fixed-point Markdown must remain parseable");
                    let mut replacements = Vec::new();
                    collect_untrusted_markdown_replacements(&ast, &mut replacements);
                    assert!(replacements.is_empty(), "{source}\n---\n{sanitized}");
                    assert!(
                        !contains_active_markdown_construct(&ast),
                        "{source}\n---\n{sanitized}"
                    );
                }
            }
        }
    }

    #[test]
    fn markdown清洗超出轮次时整篇降级为可见代码块() {
        let source = concat!(
            "<!-- comment -->\n",
            "    ![network](https://attacker.example/image.png)\n",
        );
        let sanitized = sanitize_untrusted_markdown_with_pass_limit(source, 1);
        assert_eq!(sanitized, markdown_as_indented_code(source));

        let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
            .expect("fallback Markdown must remain parseable");
        let mut replacements = Vec::new();
        collect_untrusted_markdown_replacements(&ast, &mut replacements);
        assert!(replacements.is_empty(), "{sanitized}");
        assert!(!contains_active_markdown_construct(&ast), "{sanitized}");
    }

    #[test]
    fn 已安全markdown在首轮不动点保持原文() {
        let source = concat!(
            "# 标题\n\n",
            "正文 [docs](https://example.com/docs)\n\n",
            "`<img src=\"https://example.com/code.png\">`\n",
        );
        assert_eq!(sanitize_remote_markdown(source), source);
        assert_eq!(sanitize_session_markdown(source), source);
    }

    #[test]
    fn 不安全目标降级时保留带标点的标签() {
        let sanitized = sanitize_remote_markdown(concat!(
            "[main.rs](src/main.rs)\n",
            "![截图(1).png](./a.png)\n",
        ));
        assert!(sanitized.contains(r"main\.rs"), "{sanitized}");
        assert!(sanitized.contains(r"截图\(1\)\.png"), "{sanitized}");
        assert!(!sanitized.contains("link"), "{sanitized}");
        assert!(!sanitized.contains("image"), "{sanitized}");

        let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
            .expect("sanitized labels must remain parseable");
        let mut unsafe_nodes = Vec::new();
        collect_remote_markdown_replacements(&ast, &mut unsafe_nodes);
        assert!(unsafe_nodes.is_empty(), "{sanitized}");
    }

    #[test]
    fn 会话富文本不触发任何图片或外部_html_资源() {
        let source = concat!(
            "![web](https://example.com/pixel)\n",
            "![local](file:///etc/passwd)\n",
            "![reference][image]\n",
            "[image]: https://example.com/reference.png\n",
            "[docs](https://example.com/docs)\n",
            "`<img src=\"https://example.com/code-inline.png\">`\n",
            "```html\n<img src=\"https://example.com/code-fenced.png\">\n```\n",
            r##"<img src="https://example.com/html.png"><img src="file:///etc/group"><a href="https://example.com/html">html</a><a href="#section">anchor</a>"##,
        );
        let sanitized = sanitize_session_markdown(source);
        let visible = visible_backslash_escaped_source(&sanitized);
        assert!(
            visible.contains(
                r##"<img src="https://example.com/html.png"><img src="file:///etc/group"><a href="https://example.com/html">html</a><a href="#section">anchor</a>"##,
            ),
            "raw HTML 源码应保持可见:{sanitized}"
        );
        assert!(
            sanitized.contains("`<img src=\"https://example.com/code-inline.png\">`"),
            "{sanitized}"
        );
        assert!(
            sanitized.contains("```html\n<img src=\"https://example.com/code-fenced.png\">\n```"),
            "{sanitized}"
        );
        assert!(
            sanitized.contains("[docs](https://example.com/docs)"),
            "{sanitized}"
        );

        let ast = markdown::to_mdast(&sanitized, &ParseOptions::gfm())
            .expect("sanitized session markdown must remain parseable");
        assert!(!contains_raw_markdown_html(&ast), "{sanitized}");
        let mut unsafe_nodes = Vec::new();
        collect_untrusted_markdown_replacements(&ast, &mut unsafe_nodes);
        assert!(unsafe_nodes.is_empty(), "{sanitized}");
    }

    #[test]
    fn 本地预览读取只接受限额内普通文件() {
        let dir = std::env::temp_dir().join(format!("mt-preview-http-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        let small = dir.join("small.png");
        std::fs::write(&small, b"small-image").unwrap();
        assert_eq!(
            fetch_local_preview_bytes(&small).unwrap().as_slice(),
            b"small-image"
        );
        assert!(
            fetch_local_preview_bytes(&dir).is_err(),
            "目录不得作为预览资源读取"
        );

        let oversized = dir.join("oversized.png");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(PREVIEW_IMAGE_MAX_BYTES + 1).unwrap();
        drop(file);
        assert!(
            fetch_local_preview_bytes(&oversized).is_err(),
            "超过硬上限的稀疏文件必须在读取前拒绝"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 路径比对反斜杠归一且不分大小写() {
        assert!(same_path("D:\\Git\\a.rs", "d:/git/A.RS"));
        assert!(!same_path("D:\\Git\\a.rs", "D:\\Git\\b.rs"));
        // 目录级 notify 事件里的兄弟文件不该被认成自己
        assert!(!same_path("D:/p/README.md", "D:/p/README.md.bak"));
    }

    /// **本批的钉子测试**:CRLF 文件改一个字保存,行尾一个都不许变。
    #[test]
    fn crlf_文件往返不改行尾() {
        let disk = "line1\r\nline2\r\nline3\r\n";
        assert_eq!(LineEnding::detect(disk), LineEnding::Crlf);

        // 读入:归一成 \n 喂编辑器
        let in_editor = normalize_to_lf(disk);
        assert_eq!(in_editor, "line1\nline2\nline3\n");
        assert!(!in_editor.contains('\r'), "编辑器里不留 \\r");

        // 编辑:改一个字 + 敲一次回车(gpui-component 插的是 "\n")
        let edited = in_editor.replace("line2", "LINE2") + "line4\n";

        // 写回:还原成 CRLF —— 新增的那一行也是 CRLF
        let back = restore_line_ending(&edited, LineEnding::Crlf);
        assert_eq!(back, "line1\r\nLINE2\r\nline3\r\nline4\r\n");
        assert_eq!(back.matches('\n').count(), back.matches("\r\n").count());
    }

    #[test]
    fn lf_文件不会被写成_crlf() {
        let disk = "a\nb\n";
        assert_eq!(LineEnding::detect(disk), LineEnding::Lf);
        let in_editor = normalize_to_lf(disk);
        assert_eq!(in_editor, disk);
        assert_eq!(restore_line_ending(&in_editor, LineEnding::Lf), disk);
        // 空文件 / 无换行的单行文件都算 LF
        assert_eq!(LineEnding::detect(""), LineEnding::Lf);
        assert_eq!(LineEnding::detect("no newline"), LineEnding::Lf);
    }

    #[test]
    fn 行尾还原是幂等的() {
        // 万一有 \r\n 混进编辑器,还原两次也不该变成 \r\r\n
        let once = restore_line_ending("a\r\nb", LineEnding::Crlf);
        let twice = restore_line_ending(&once, LineEnding::Crlf);
        assert_eq!(once, "a\r\nb");
        assert_eq!(twice, once);
    }

    #[test]
    fn 语言按扩展名映射到组件库认得的名字() {
        assert_eq!(language_for("main.rs"), "rust");
        assert_eq!(language_for("D:\\p\\src\\store.ts"), "typescript");
        assert_eq!(language_for("App.tsx"), "tsx");
        assert_eq!(language_for("index.JS"), "javascript", "大小写不敏感");
        assert_eq!(language_for("Cargo.toml"), "toml");
        assert_eq!(language_for("config.yml"), "yaml");
        assert_eq!(language_for("a.jsonc"), "json");
        assert_eq!(language_for("run.sh"), "bash");
        assert_eq!(language_for("a.hpp"), "cpp");
        assert_eq!(language_for("a.h"), "c");
        // 特殊文件名压扩展名
        assert_eq!(language_for("Makefile"), "make");
        assert_eq!(language_for("CMakeLists.txt"), "cmake");
        assert_eq!(language_for("Dockerfile"), "bash");
        // 认不出 → 纯文本(原版「匹配不到就是纯文本」)
        assert_eq!(language_for("notes.xyz"), "text");
        assert_eq!(language_for("LICENSE"), "text");
    }

    #[test]
    fn 映射出来的语言名组件库全都认得() {
        // 认不得会静默退成 Plain,画出来没有高亮而编译期无感 —— 用它自己的
        // `from_str` 钉住:除了 "text",每个名字都要落到非 Plain 的分支
        use gpui_component::highlighter::Language;
        for name in [
            "rust", "typescript", "tsx", "javascript", "json", "python", "go", "ruby", "java",
            "csharp", "c", "cpp", "css", "html", "bash", "toml", "yaml", "markdown", "sql",
            "swift", "zig", "elixir", "scala", "proto", "graphql", "diff", "cmake", "ejs", "erb",
            "make",
        ] {
            assert_ne!(
                Language::from_str(name).name(),
                Language::Plain.name(),
                "组件库不认得语言名 {name}"
            );
        }
        assert_eq!(Language::from_str("text").name(), Language::Plain.name());
    }

    #[test]
    fn 命中行定位拒绝越界行号() {
        let text = "a\nb\nc\n";
        assert_eq!(highlight_target(Some(2), text), Some(2));
        assert_eq!(highlight_target(Some(3), text), Some(3));
        // 越界不动(原版 `highlightLine > doc.lines` 直接 return)
        assert_eq!(highlight_target(Some(9), text), None);
        assert_eq!(highlight_target(Some(0), text), None, "行号是 1-based");
        // 文件树那条路压根不给行号
        assert_eq!(highlight_target(None, text), None);
        // 空文件也算有第 1 行
        assert_eq!(highlight_target(Some(1), ""), Some(1));
    }

    #[test]
    fn 四种渲染分支的判定顺序() {
        // 图片先于一切:原版图片分支压根不读文件
        assert_eq!(branch_of(true, true, false, None), Branch::Image);
        assert_eq!(branch_of(false, true, false, None), Branch::Loading);
        assert_eq!(branch_of(false, false, true, None), Branch::Error);

        let mut binary = result("");
        binary.is_binary = true;
        let mut large = result("");
        large.too_large = true;
        // 二进制先于过大 —— 二进制文件的 content 也是空的,顺序换了会显示成「文件过大」
        assert_eq!(branch_of(false, false, false, Some(&binary)), Branch::Binary);
        assert_eq!(branch_of(false, false, false, Some(&large)), Branch::TooLarge);
        assert_eq!(branch_of(false, false, false, Some(&result("x"))), Branch::Editor);
        // 读完了但既没结果也没错(不该发生)按 loading 处理,不画空编辑器
        assert_eq!(branch_of(false, false, false, None), Branch::Loading);
    }

    #[test]
    fn 三种不可编辑的情况都不画编辑器() {
        let mut binary = result("");
        binary.is_binary = true;
        let mut large = result("");
        large.too_large = true;
        assert!(!can_edit(true, Some(&result("x"))), "图片");
        assert!(!can_edit(false, Some(&binary)), "二进制");
        assert!(!can_edit(false, Some(&large)), "过大");
        assert!(!can_edit(false, None), "还没读到");
        assert!(can_edit(false, Some(&result("x"))));
    }

    /// 后端的两道防线(1MB 上限 / 非 UTF-8 即二进制)与前端分支合起来跑一遍真磁盘。
    #[test]
    fn 二进制与超限探测走真文件() {
        let dir = std::env::temp_dir().join(format!("mt-fv-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // 非 UTF-8 → is_binary
        let bin = dir.join("bin.dat");
        std::fs::write(&bin, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let res = mt_project::fs::read_file_content(&dir, &bin).unwrap();
        assert!(res.is_binary && !res.too_large);
        assert_eq!(branch_of(false, false, false, Some(&res)), Branch::Binary);
        assert!(!can_edit(false, Some(&res)));

        // > 1MB → too_large(且 content 为空)
        let big = dir.join("big.txt");
        std::fs::write(&big, vec![b'a'; (mt_project::fs::MAX_FILE_VIEW_SIZE + 1) as usize]).unwrap();
        let res = mt_project::fs::read_file_content(&dir, &big).unwrap();
        assert!(res.too_large && !res.is_binary && res.content.is_empty());
        assert_eq!(branch_of(false, false, false, Some(&res)), Branch::TooLarge);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 保存路径语义:走 `mt_project::fs::write_file_content`(内部原子写),
    /// 且 CRLF 文件读→改→写一整圈之后磁盘字节里的行尾一个都没变。
    #[test]
    fn 保存走原子写且_crlf_全程不变() {
        let dir = std::env::temp_dir().join(format!("mt-fv-save-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("crlf.txt");
        std::fs::write(&file, b"alpha\r\nbeta\r\n").unwrap();

        // 读:后端给的是原文(带 \r\n)
        let res = mt_project::fs::read_file_content(&dir, &file).unwrap();
        assert!(res.content.contains("\r\n"), "后端不做行尾归一,归一在 UI 侧");
        let ending = LineEnding::detect(&res.content);
        let editor_text = normalize_to_lf(&res.content);

        // 改 + 敲回车
        let edited = editor_text.replace("beta", "BETA") + "gamma\n";

        // 写
        mt_project::fs::write_file_content(&dir, &file, &restore_line_ending(&edited, ending))
            .unwrap();

        let on_disk = std::fs::read(&file).unwrap();
        assert_eq!(on_disk, b"alpha\r\nBETA\r\ngamma\r\n");
        // 原子写不留临时文件
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "原子写的临时文件必须已经被 rename 掉");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
