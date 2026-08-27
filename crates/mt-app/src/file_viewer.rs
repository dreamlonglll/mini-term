//! 文件预览与内置编辑器。对应 `src/components/FileViewerModal.tsx`(498 行)
//! 与 `src/components/CodeEditor.tsx`(350 行),审计缺口 #29。
//!
//! # 一个单例,两个入口
//!
//! 原版是**两处各挂一份** `FileViewerModal`(`FileTree.tsx:838` / `SearchModal.tsx:356`,
//! 各自 `lazy()` 懒加载,理由是「CodeMirror + react-markdown 数百 KB」)。GPUI 没有
//! 代码分割这回事,这里做成**单例**:文件树单击文件行、全局搜索单击结果都调
//! [`open`],走 [`crate::prompt::open_guarded`] + [`crate::overlay::kind::FILE_VIEWER`]。
//! 于是防叠开、Esc、快捷键让路一次到位。
//!
//! 「已经开着时再点一条搜索结果」**不是叠开而是换文件** —— 原版靠 `filePath` prop
//! 变化触发 `setCurrentPath(filePath)`(`FileViewerModal.tsx:239-242`),这里由
//! [`open`] 认出栈里已有自己、转而 [`FileViewer::navigate`]。注意原版那条路
//! **不问「有未保存修改吗」**(effect 直接重设 currentPath),照抄。
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
//!    于是原版三条链接处置(外链弹确认框 / 文档内锚点滚动 / 本地文件在弹窗内跳转)
//!    都做不到,**弹窗内跳转历史栈(`←` 返回)随之整条不做**。记档。
//! 2. **HTML 是简版渲染,不是浏览器**:GPUI 侧没有 iframe 等价物,`TextView::html`
//!    与 markdown 那支是同一个富文本渲染器(无 CSS / 无 JS)。此处曾按规格 B.6.3
//!    的建议「只留源码编辑器」,**已翻案**(用户要求):现在给预览态,但配一条
//!    说明 + 工具栏常驻「用浏览器打开」——走样的排版有解释、真效果有出口,
//!    比对着一屏源码有用。相对资源不再是问题,见 [`rewrite_html_urls`]。
//! 3. **遮罩点击不关窗**:Dialog 的遮罩关闭无法拦截,而关闭要先过「未保存确认」——
//!    留着就等于给草稿开了一条静默丢弃的路。改为只能 ✕ / Esc 关。

use std::cell::RefCell;
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
    StatefulInteractiveElement, Styled, StyledImage as _, Subscription, Task, WeakEntity, Window,
    div, img, prelude::FluentBuilder as _, px,
};
use gpui::http_client::{
    AsyncBody, HttpClient, Request, Response, StatusCode, Url, http::HeaderValue,
};
use gpui_component::ActiveTheme as _;
use gpui_component::WindowExt as _;
use gpui_component::input::{Input, InputEvent, InputState, Position, Search};
use gpui_component::text::{TextView, TextViewStyle};
use mt_ui::tooltip::Tooltip;
use mt_project::fs::FileContentResult;
use mt_project::watch::FsWatcher;
use mt_ui::icons::FileIcon;

use crate::i18n::t;
use crate::overlay::kind;
use crate::prompt::{Confirm, close_guarded, open_guarded};
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewerHost {
    Modal,
    Workbench,
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
/// 两道闸都来自原版:
/// - `same_file`:跳走之后行号就失效了(`FileViewerModal.tsx:486` 的
///   `highlightLine={currentPath === filePath ? highlightLine : undefined}`);
///   本批没有弹窗内跳转(见模块注释偏差 1),但「预览器已开着又点了另一条搜索结果」
///   会换掉 `origin_path`,这道闸照样要有;
/// - 越界不动(`CodeEditor.tsx:341` 的 `if (highlightLine > view.state.doc.lines) return`)。
pub fn highlight_target(highlight_line: Option<u32>, same_file: bool, text: &str) -> Option<u32> {
    if !same_file {
        return None;
    }
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
// 相对路径按当前文件所在目录解析成 `Resource::Path`,见 [`parse_image_line`]
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

/// markdown 里的一张图片。整行只有图片时被拆出来自绘(见 [`parse_image_line`])。
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

/// 把 markdown 源切成**块级**段:GFM 表格与整行图片各自独立成段,其余文本
/// 按空行拆块(围栏代码块 ``` / ~~~ 内的空行、竖线、图片语法都不拆)。
///
/// 逐块喂 TextView 而不是整篇 —— 除了表格要自绘,还有一条硬理由:
/// gpui-component 0.5.1 的非虚拟化路径把 `is_last: true` 原样传给 Root 的
/// **每个**子块(`node.rs:1150-1156`,ListState 路径才逐块算),而
/// `is_last → paragraph_gap = 0`,整篇喂进去相邻段落会贴死(用户对照原版
/// 实测)。块间距改由 [`block_top_margin`] 自己控,顺带复刻原版「标题前
/// 间距更大」的非对称节奏(`.md-preview h* { margin-top: 1.4em }`)。
fn split_md_blocks(source: &str) -> Vec<MdSegment> {
    let lines: Vec<&str> = source.lines().collect();
    let mut segs = Vec::new();
    let mut text_start = 0usize;
    let mut in_fence = false;
    let mut i = 0usize;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            i += 1;
            continue;
        }
        if in_fence {
            i += 1;
            continue;
        }
        // 空行 = 块边界(围栏外)
        if lines[i].trim().is_empty() {
            push_text_block(&lines[text_start..i], &mut segs);
            text_start = i + 1;
            i += 1;
            continue;
        }
        if lines[i].contains('|')
            && i + 1 < lines.len()
            && let Some(aligns) = parse_separator(lines[i + 1])
        {
            let header = split_cells(lines[i]);
            // GFM 规则:分隔行列数与表头一致才成表
            if !header.is_empty() && header.len() == aligns.len() {
                push_text_block(&lines[text_start..i], &mut segs);
                let mut rows = Vec::new();
                let mut j = i + 2;
                while j < lines.len() && lines[j].contains('|') && !lines[j].trim().is_empty() {
                    let mut cells = split_cells(lines[j]);
                    cells.resize(header.len(), String::new());
                    rows.push(cells);
                    j += 1;
                }
                segs.push(MdSegment::Table(MdTable { header, aligns, rows }));
                text_start = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    push_text_block(&lines[text_start..], &mut segs);
    segs
}

/// 收一个文本块,顺手把**整行只有图片**的行拆成 [`MdSegment::Images`] 自绘。
///
/// 图片行不必单独成段(README 里「一段说明紧跟一张截图」中间常常没有空行),
/// 所以切分落在这一层而不是 [`split_md_blocks`] 的块级循环里。围栏代码块
/// 内的行不参与 —— ` ```md ` 示例里的 `![](…)` 是代码,不是图片。
fn push_text_block(lines: &[&str], segs: &mut Vec<MdSegment>) {
    let mut start = 0usize;
    let mut in_fence = false;
    for (ix, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let Some(images) = parse_image_line(line) else {
            continue;
        };
        push_plain_text(&lines[start..ix], segs);
        segs.push(MdSegment::Images(images));
        start = ix + 1;
    }
    push_plain_text(&lines[start..], segs);
}

/// 非空才算一块(连续空行 / 段与表格间的空隙都会产生空切片)。
fn push_plain_text(lines: &[&str], segs: &mut Vec<MdSegment>) {
    if lines.iter().any(|l| !l.trim().is_empty()) {
        segs.push(MdSegment::Text(lines.join("\n")));
    }
}

/// 一行**只有图片**时解析出其中的图片,否则 `None`(那一行照旧交给 TextView)。
///
/// 认的形态就是 README 里图片的常见写法:`![alt](url)`、带 title 的
/// `![alt](url "标题")`、当链接用的 `[![alt](url)](link)`,以及同一行并排多张
/// (徽章行)。**行里混了文字就整行放弃** —— 拆一半会把段落切碎,而内联图片
/// 本来就是少数派。
///
/// 已知不覆盖(记档,不修):列表项 `- ![a](b)`、引用块 `> ![a](b)`、表格格子
/// 里的图片 —— 这些行有前缀语法,拆出来会毁掉列表/引用结构,仍走 TextView
/// (于是本地路径图片在那些位置依旧不显示)。
fn parse_image_line(line: &str) -> Option<Vec<MdImage>> {
    // 四个空格 / 制表符缩进是代码块,里面的图片语法是代码
    if line.starts_with("    ") || line.starts_with('\t') {
        return None;
    }
    let mut rest = line.trim();
    if rest.is_empty() {
        return None;
    }
    let mut images = Vec::new();
    while !rest.is_empty() {
        let (image, tail) = parse_image_at(rest)?;
        images.push(image);
        rest = tail.trim_start();
    }
    (!images.is_empty()).then_some(images)
}

/// 从开头吃掉一张图片(可带外层链接),返回图片与剩余部分。
fn parse_image_at(s: &str) -> Option<(MdImage, &str)> {
    // `[![alt](img)](link)`:方括号里必须**整体**是一张图片,别的都不认
    if let Some(after) = s.strip_prefix('[') {
        let (inner, tail) = take_balanced(after, '[', ']')?;
        let (mut image, inner_tail) = parse_bang_image(inner.trim())?;
        if !inner_tail.trim().is_empty() {
            return None;
        }
        let tail = tail.strip_prefix('(')?;
        let (dest, tail) = take_balanced(tail, '(', ')')?;
        let (link, _) = split_dest(dest);
        if link.is_empty() {
            return None;
        }
        image.link = Some(link);
        return Some((image, tail));
    }
    parse_bang_image(s)
}

/// `![alt](url "title")`。
fn parse_bang_image(s: &str) -> Option<(MdImage, &str)> {
    let after = s.strip_prefix("![")?;
    let (alt, tail) = take_balanced(after, '[', ']')?;
    let tail = tail.strip_prefix('(')?;
    let (dest, tail) = take_balanced(tail, '(', ')')?;
    let (url, title) = split_dest(dest);
    if url.is_empty() {
        return None;
    }
    Some((
        MdImage {
            url,
            alt: alt.trim().to_string(),
            title,
            link: None,
        },
        tail,
    ))
}

/// 吃到与开头配对的 `close`(允许嵌套、`\` 转义),入参是**开符之后**的部分,
/// 返回 `(括号内, 闭合符之后)`;没配上返回 `None`。
fn take_balanced(s: &str, open: char, close: char) -> Option<(&str, &str)> {
    let mut depth = 1usize;
    let mut escaped = false;
    for (ix, ch) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Some((&s[..ix], &s[ix + ch.len_utf8()..]));
                }
            }
            _ => {}
        }
    }
    None
}

/// 括号里的目标 → `(url, title)`。认 `<路径 带空格>` 与 `url "标题"` 两种写法。
fn split_dest(dest: &str) -> (String, Option<String>) {
    let d = dest.trim();
    if let Some(rest) = d.strip_prefix('<')
        && let Some((url, tail)) = rest.split_once('>')
    {
        return (url.trim().to_string(), title_of(tail));
    }
    match d.find(char::is_whitespace) {
        Some(cut) => (d[..cut].to_string(), title_of(&d[cut..])),
        None => (d.to_string(), None),
    }
}

/// 目标后面那截 → title(剥 `"`/`'`/`(`)。空的算没有。
fn title_of(tail: &str) -> Option<String> {
    let t = tail
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '(' || c == ')')
        .trim();
    (!t.is_empty()).then(|| t.to_string())
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
/// 其余 scheme 一律拒绝。出网那支有两条硬约束:**10s 超时**(`reqwest::blocking`
/// 默认无限等)与 **32MB 上限**(坏 URL 不该把内存拖垮),详见 [`fetch_remote_bytes`]。
pub struct PreviewHttpClient;

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
                    std::fs::read(&path).with_context(|| format!("读不到 {}", path.display()))?
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
    /// 单张图片的字节上限。超了宁可不画,也不把几百 MB 读进内存
    const MAX_BYTES: u64 = 32 * 1024 * 1024;
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
        anyhow::ensure!(len <= MAX_BYTES, "图片过大({len} 字节): {url}");
    }
    let mut body = Vec::new();
    resp.take(MAX_BYTES + 1).read_to_end(&mut body)?;
    anyhow::ensure!(body.len() as u64 <= MAX_BYTES, "图片过大: {url}");
    Ok(body)
}

/// 把 md 源里图片的**本地**目标改写成 `file:///…` 绝对 URL。
///
/// 整行只有图片的那些行由 [`FileViewer::render_md_images`] 自绘、不经过这里;
/// 这条是给**内联**图片兜底的(列表项 `- ![a](b)`、引用块、表格格子里的图片)——
/// 它们要走 TextView,而那条路只认网络 URI,配上 [`PreviewHttpClient`] 才画得出来。
///
/// 围栏代码块与行内 code 里的图片语法是**代码不是图片**,原样留着。
fn rewrite_md_image_urls(source: &str, base_dir: &Path) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_fence = false;
    for (ix, line) in source.split('\n').enumerate() {
        if ix > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        // 围栏内 / 四空格缩进的代码块原样
        if in_fence || line.starts_with("    ") || line.starts_with('\t') {
            out.push_str(line);
            continue;
        }
        rewrite_md_line_into(line, base_dir, &mut out);
    }
    out
}

/// Remote rich-text is untrusted input from another machine. Do not let images
/// hand `file://` (or an implicit local path) to the process-wide preview HTTP
/// client, and do not let ordinary links pass local paths to `cx.open_url`.
/// Explicit HTTP(S) resources remain available; SSH-relative assets are deferred.
fn sanitize_remote_markdown_images(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_fence = false;
    for (ix, line) in source.split('\n').enumerate() {
        if ix > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence || line.starts_with("    ") || line.starts_with('\t') {
            out.push_str(line);
            continue;
        }

        let mut rest = line;
        while let Some(ch) = rest.chars().next() {
            if ch == '`' {
                if let Some(consumed) = markdown_code_span_len(rest) {
                    out.push_str(&rest[..consumed]);
                    rest = &rest[consumed..];
                } else {
                    // An unmatched backtick is ordinary text in CommonMark;
                    // it must not suppress sanitization for the rest of the line.
                    out.push('`');
                    rest = &rest[1..];
                }
                continue;
            }
            if rest.starts_with("![")
                && let Some((image, tail)) = parse_bang_image(rest)
            {
                let url = image.url.trim().to_ascii_lowercase();
                if url.starts_with("http://") || url.starts_with("https://") {
                    let consumed = rest.len() - tail.len();
                    out.push_str(&rest[..consumed]);
                } else {
                    out.push('[');
                    out.push_str(if image.alt.is_empty() {
                        "image"
                    } else {
                        &image.alt
                    });
                    out.push(']');
                }
                rest = tail;
                continue;
            }
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    sanitize_remote_markdown_reference_urls(&sanitize_remote_markdown_inline_links(&out))
}

/// Length in bytes of a complete CommonMark-style backtick code span at the
/// start of `source`. Closing delimiters must contain exactly the same number
/// of backticks; an unmatched opener returns `None` and is treated as text by
/// the sanitizers.
fn markdown_code_span_len(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.first() != Some(&b'`') {
        return None;
    }
    let opening = bytes.iter().take_while(|byte| **byte == b'`').count();
    let mut at = opening;
    while at < bytes.len() {
        if bytes[at] != b'`' {
            at += 1;
            continue;
        }
        let start = at;
        while at < bytes.len() && bytes[at] == b'`' {
            at += 1;
        }
        if at - start == opening {
            return Some(at);
        }
    }
    None
}

fn remote_markdown_url_allowed(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with("mailto:")
        || lower.starts_with("tel:")
        || lower.starts_with('#')
}

fn sanitized_remote_markdown_link_label(label: &str) -> String {
    let trimmed = label.trim();
    if let Some((image, tail)) = parse_bang_image(trimmed)
        && tail.trim().is_empty()
    {
        let lower = image.url.trim().to_ascii_lowercase();
        if lower.starts_with("http://") || lower.starts_with("https://") {
            return trimmed.to_string();
        }
    }
    if !trimmed.is_empty()
        && trimmed
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch.is_whitespace() || matches!(ch, '_' | '-'))
    {
        trimmed.to_string()
    } else {
        "link".into()
    }
}

/// Markdown links use the same rich-text click path as local documents, which
/// ultimately calls `cx.open_url`. Remote content must not hand that path a
/// local `file://` URI, a relative path, or another arbitrary scheme. Preserve
/// the visible label while replacing disallowed inline destinations and URI
/// autolinks with plain text. Fenced and inline code stay byte-for-byte
/// unchanged.
fn sanitize_remote_markdown_inline_links(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_fence = false;
    for (ix, line) in source.split('\n').enumerate() {
        if ix > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        if in_fence || line.starts_with("    ") || line.starts_with('\t') {
            out.push_str(line);
            continue;
        }

        let mut rest = line;
        while let Some(ch) = rest.chars().next() {
            if ch == '`' {
                if let Some(consumed) = markdown_code_span_len(rest) {
                    out.push_str(&rest[..consumed]);
                    rest = &rest[consumed..];
                } else {
                    out.push('`');
                    rest = &rest[1..];
                }
                continue;
            }
            if rest.starts_with('[')
                && let Some((label, after_label)) = take_balanced(&rest[1..], '[', ']')
                && let Some(after_open) = after_label.strip_prefix('(')
                && let Some((destination, tail)) = take_balanced(after_open, '(', ')')
            {
                let (url, _title) = split_dest(destination);
                if remote_markdown_url_allowed(&url) {
                    let consumed = rest.len() - tail.len();
                    out.push_str(&rest[..consumed]);
                } else {
                    out.push_str(&sanitized_remote_markdown_link_label(label));
                }
                rest = tail;
                continue;
            }
            if ch == '<'
                && let Some(end) = rest[1..].find('>')
            {
                let value = &rest[1..end + 1];
                if scheme_len(value).is_some() && !remote_markdown_url_allowed(value) {
                    out.push_str("link");
                } else {
                    out.push_str(&rest[..end + 2]);
                }
                rest = &rest[end + 2..];
                continue;
            }
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

/// Reference-style images resolve their URL from a separate `[label]: target`
/// definition, so the inline `![alt](target)` scanner above never sees it.
/// Sanitize every non-code reference definition to the same HTTP(S)-only policy;
/// relative reference links are not part of the first remote-preview contract.
fn sanitize_remote_markdown_reference_urls(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut in_fence = false;
    for (ix, line) in source.split('\n').enumerate() {
        if ix > 0 {
            out.push('\n');
        }
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push_str(line);
            continue;
        }
        let indent = line.len() - trimmed.len();
        if in_fence || line.starts_with('\t') || indent > 3 || !trimmed.starts_with('[') {
            out.push_str(line);
            continue;
        }
        let Some((_label, after_label)) = take_balanced(&trimmed[1..], '[', ']') else {
            out.push_str(line);
            continue;
        };
        let Some(after_colon) = after_label.strip_prefix(':') else {
            out.push_str(line);
            continue;
        };
        let destination = after_colon.trim_start();
        let destination_start = line.len() - destination.len();
        let (url, destination_len, bracketed) = if let Some(rest) = destination.strip_prefix('<') {
            let Some(end) = rest.find('>') else {
                out.push_str(line);
                continue;
            };
            (&rest[..end], end + 2, true)
        } else {
            let len = destination
                .find(char::is_whitespace)
                .unwrap_or(destination.len());
            (&destination[..len], len, false)
        };
        let url = url.trim().to_ascii_lowercase();
        if url.starts_with("http://") || url.starts_with("https://") {
            out.push_str(line);
            continue;
        }
        out.push_str(&line[..destination_start]);
        if bracketed {
            out.push_str("<about:blank>");
        } else {
            out.push_str("about:blank");
        }
        out.push_str(&line[destination_start + destination_len..]);
    }
    out
}

/// 一行里的图片目标逐个改写(行内 code 跳过)。
fn rewrite_md_line_into(line: &str, base_dir: &Path, out: &mut String) {
    let mut rest = line;
    let mut in_code = false;
    while let Some(ch) = rest.chars().next() {
        if ch == '`' {
            in_code = !in_code;
            out.push('`');
            rest = &rest[1..];
            continue;
        }
        if !in_code
            && rest.starts_with("![")
            && let Some((image, tail)) = parse_bang_image(rest)
        {
            let url = match resolve_image_src(&image.url, base_dir) {
                MdImageSrc::Local(path) => to_file_url(&path).unwrap_or(image.url.clone()),
                _ => image.url.clone(),
            };
            out.push_str("![");
            out.push_str(&image.alt);
            out.push_str("](");
            out.push_str(&url);
            if let Some(title) = &image.title {
                out.push_str(" \"");
                out.push_str(title);
                out.push('"');
            }
            out.push(')');
            rest = tail;
            continue;
        }
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
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
    while let Some((value_start, value_end, _attr)) = find_next_url_attr(&lower, pos) {
        out.push_str(&source[pos..value_start]);
        out.push_str(&rewrite_html_value(&source[value_start..value_end], base_dir));
        pos = value_end;
    }
    out.push_str(&source[pos..]);
    out
}

/// 找下一个 `src=` / `href=` / `poster=` 的值区间。HTML 允许属性值不加引号，
/// 所以这里同时覆盖 `src="x"`、`src='x'` 与 `src=x`；远程内容的安全过滤
/// 不能只认前两种，否则 html5ever 仍会把第三种解析成可加载资源。
fn find_next_url_attr(lower: &str, from: usize) -> Option<(usize, usize, &'static str)> {
    const ATTRS: [&str; 3] = ["src", "href", "poster"];
    let mut best: Option<(usize, usize, usize, &'static str)> = None;
    for attr in ATTRS {
        let mut at = from;
        while let Some(rel) = lower[at..].find(attr) {
            let name_start = at + rel;
            at = name_start + attr.len();
            // HTML5 error recovery also accepts attributes after a stray `/`
            // (`<img/src=x>`) or immediately after a quoted attribute
            // (`alt="x"src=y`). Cover those parser forms while still rejecting
            // compound names such as `data-src` and `xlink:href`.
            if !lower[..name_start]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_whitespace() || matches!(ch, '/' | '"' | '\''))
            {
                continue;
            }
            // 后面得是 `\s*=\s*`，值可以带引号也可以不带
            let after_name = &lower[at..];
            let trimmed = after_name.trim_start();
            let Some(after_eq) = trimmed.strip_prefix('=') else {
                continue;
            };
            let value = after_eq.trim_start();
            // `after_name` 与 `value` 的长度差,正好是「空白 + `=` + 空白」那一截
            let value_at = at + (after_name.len() - value.len());
            let (value_start, value_end) = match value.chars().next() {
                Some(quote @ ('"' | '\'')) => {
                    let value_start = value_at + quote.len_utf8();
                    let Some(rel) = lower[value_start..].find(quote) else {
                        // A stray `href="` in ordinary text must not stop the
                        // scanner before a later, valid `<img src=...>`.
                        at = value_start;
                        continue;
                    };
                    (value_start, value_start + rel)
                }
                Some(_) => {
                    let rel = value
                        .find(|ch: char| ch.is_whitespace() || ch == '>')
                        .unwrap_or(value.len());
                    (value_at, value_at + rel)
                }
                None => (value_at, value_at),
            };
            let candidate = (name_start, value_start, value_end, attr);
            if best.is_none_or(|(best_start, _, _, _)| candidate.0 < best_start) {
                best = Some(candidate);
            }
            break;
        }
    }
    best.map(|(_, value_start, value_end, attr)| (value_start, value_end, attr))
}

/// Remote HTML may load explicit HTTP(S) resources and expose explicit
/// web/mail/tel/fragment links. Local file URLs, relative paths and script-like
/// schemes must never reach the process-wide preview HTTP client.
fn sanitize_remote_html_urls(source: &str) -> String {
    let lower = source.to_ascii_lowercase();
    let mut out = String::with_capacity(source.len());
    let mut pos = 0usize;
    while let Some((value_start, value_end, attr)) = find_next_url_attr(&lower, pos) {
        let value = source[value_start..value_end].trim();
        let value_lower = value.to_ascii_lowercase();
        let is_web = ["http:", "https:"]
            .iter()
            .any(|prefix| value_lower.starts_with(prefix));
        let replacement = match attr {
            "href"
                if value.starts_with('#')
                    || is_web
                    || ["mailto:", "tel:"]
                        .iter()
                        .any(|prefix| value_lower.starts_with(prefix)) =>
            {
                &source[value_start..value_end]
            }
            "src" | "poster" if is_web => &source[value_start..value_end],
            "href" => "#",
            _ => "about:blank",
        };
        out.push_str(&source[pos..value_start]);
        out.push_str(replacement);
        pos = value_end;
    }
    out.push_str(&source[pos..]);
    out
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
        .child(label)
        .when_some(hint, |el, hint| {
            el.tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
        })
        .when_some(open, |el, url| {
            el.cursor_pointer()
                .hover(|el| el.text_color(ui::text_primary()))
                .on_click(move |_: &ClickEvent, _window, cx| cx.open_url(&url))
        })
        .into_any_element()
}

// ─── 单例句柄 ─────────────────────────────────────────────────

thread_local! {
    /// 当前开着的那一个。[`open`] 用它认出「已经开着 → 换文件而不是叠开」。
    ///
    /// 与 [`crate::overlay`] 同一个理由用 `thread_local`:gpui 的视图全在主线程上。
    /// **弱引用**:强引用会把视图连同它的目录监听一起吊在这张表上 ——
    /// 除了我们自己那条关闭路,`Root` 层清空对话框栈之类的路径不会来这里摘表,
    /// 于是 watcher 永远不释放。弱引用则是「谁真的还开着谁说了算」。
    static CURRENT: RefCell<Option<WeakEntity<FileViewer>>> = const { RefCell::new(None) };
}

/// 打开预览器。两个入口(文件树单击文件行、全局搜索单击结果)共用。
///
/// `highlight_line` 是 1-based 行号(全局搜索的命中行);文件树那条路给 `None`。
pub fn open(
    project_root: PathBuf,
    path: PathBuf,
    highlight_line: Option<u32>,
    window: &mut Window,
    cx: &mut App,
) {
    // 已经开着 → 换文件(原版是 `filePath` prop 变化那条 effect,不问未保存)
    let existing = CURRENT
        .with(|c| c.borrow().clone())
        .filter(|_| crate::overlay::contains(crate::overlay::key(kind::FILE_VIEWER)))
        .and_then(|weak| weak.upgrade());
    if let Some(view) = existing {
        view.update(cx, |this, cx| {
            this.navigate(project_root, path, highlight_line, window, cx)
        });
        return;
    }

    // 守卫要在**建视图之前**判:被 `open_guarded` 拦下时视图已经建好、
    // 焦点也已经排上了,而它永远不会被画出来(与 `search_modal::open` 同一个坑)
    if crate::overlay::contains(crate::overlay::key(kind::FILE_VIEWER)) {
        return;
    }

    let source = DocumentSource::Local {
        project_id: format!("modal:{}", project_root.to_string_lossy()),
        project_root,
        path,
    };
    let view = cx.new(|cx| FileViewer::new(source, highlight_line, ViewerHost::Modal, window, cx));
    CURRENT.with(|c| *c.borrow_mut() = Some(view.downgrade()));

    open_guarded(kind::FILE_VIEWER, window, cx, {
        let view = view.clone();
        move |dialog, window, _cx| {
            let viewport = window.viewport_size();
            // 原版 `w-[90vw] h-[80vh]` + `align="center"`
            dialog
                .p_0()
                // 工具栏右侧有自己的 ✕;`close_button` 画的是 `IconName::Close`,
                // 而 0.5.1 不带 svg 资产(渲染成空白且编译期无感)
                .close_button(false)
                // 遮罩点击关闭**关掉**:它绕不过「有未保存修改吗」那道确认
                // (Dialog 没给拦截口),留着等于给草稿开一条静默丢弃的路
                .overlay_closable(false)
                // Esc 也自己接:`keyboard(true)` 时 Dialog 把 escape 绑成 `Cancel`
                // 动作,动作**先于** `on_key_down` 派发且会吃掉事件
                // (gpui `window.rs:3834-3846`:先跑 bindings,propagate 为假就 return),
                // 我们的两段式退出就再也收不到 Esc 了
                .keyboard(false)
                .w(viewport.width * 0.9)
                // Dialog 只认「距顶多少」,居中就是 (100% - 80%) / 2
                .margin_top(viewport.height * 0.1)
                .child(div().h(viewport.height * 0.8).child(view.clone()))
        }
    });

    // Dialog 打开时会把焦点抢到自己面板上,聚焦要排在它后面
    window.defer(cx, move |window, cx| {
        view.update(cx, |this, cx| this.focus_content(window, cx));
    });
}

/// 关掉(✕ / Esc 确认之后)。
fn close(window: &mut Window, cx: &mut App) {
    CURRENT.with(|c| *c.borrow_mut() = None);
    close_guarded(kind::FILE_VIEWER, window, cx);
}

// ─── 视图 ─────────────────────────────────────────────────────

pub struct FileViewer {
    source: DocumentSource,
    host: ViewerHost,
    project_root: PathBuf,
    /// 外部传进来的那一个。`highlight_line` 只在 `current == origin` 时生效
    /// (`FileViewerModal.tsx:486`:跳走之后行号就失效了)。
    origin_path: PathBuf,
    current_path: PathBuf,
    highlight_line: Option<u32>,

    loading: bool,
    remote_refreshing: bool,
    error: Option<String>,
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
        Self::new(source, highlight_line, ViewerHost::Workbench, window, cx)
    }

    fn new(
        source: DocumentSource,
        highlight_line: Option<u32>,
        host: ViewerHost,
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
            host,
            project_root,
            origin_path: path.clone(),
            current_path: path,
            highlight_line,
            loading: false,
            remote_refreshing: false,
            error: None,
            result: None,
            editor: None,
            saved: String::new(),
            disk: String::new(),
            preview_draft: None,
            line_ending: LineEnding::Lf,
            md_cache: RefCell::new(None),
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

    /// 换一个文件(单例被复用时)。**不问未保存修改** —— 原版那条 effect
    /// (`FileViewerModal.tsx:239-242`)也不问。
    fn navigate(
        &mut self,
        project_root: PathBuf,
        path: PathBuf,
        highlight_line: Option<u32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        self.source = DocumentSource::Local {
            project_id: format!("modal:{}", project_root.to_string_lossy()),
            project_root: project_root.clone(),
            path: path.clone(),
        };
        self.project_root = project_root;
        self.origin_path = path.clone();
        self.current_path = path;
        self.highlight_line = highlight_line;
        self.preview = true;
        self.remote_source_invalid = false;
        self.reload(window, cx);
        self.focus_content(window, cx);
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

    /// 「预览 / 源码」段控件的显示条件:`(isMd || isHtml) && canEdit`
    /// (`FileViewerModal.tsx:355`,与原版同口径)。
    ///
    /// HTML 那一半曾经被摘掉(模块注释偏差 2 的旧结论:没有 iframe 等价物,
    /// 富文本渲染器画出来的东西「比不提供更误导人」)。现在**改为提供** ——
    /// 见 [`Self::render_html`]:简版渲染 + 顶上一条说明 + 工具栏常驻
    /// 「用浏览器打开」,把真效果的出口摆明,比只给一屏源码有用。
    fn has_preview_toggle(&self) -> bool {
        let path = self.path_str();
        (is_markdown_file(&path) || is_html_file(&path))
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
                            view.apply_remote_content(content, window, cx);
                        }
                    }
                    Err(error) => {
                        view.error = Some(error);
                        if view.can_take_async_focus(window, cx) {
                            view.focus.focus(window);
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
            let same_file = self.current_path == self.origin_path;
            if let Some(line) = highlight_target(self.highlight_line, same_file, &text) {
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
        if let Some(line) = highlight_target(highlight_line, true, &text) {
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

    /// 两段式退出的第二段:有未保存修改先问一句(`FileViewerModal.tsx:153-164`)。
    ///
    /// 第一段(编辑器搜索面板开着时 Esc 只关面板)是 GPUI **结构性免费**的:
    /// gpui-component 的搜索面板把 `escape` 绑在自己的 `Input` 上下文里、
    /// `on_action_escape` 不 `cx.propagate()`(`input/search.rs:305-307`),
    /// 焦点在面板上时 Esc 被它吃掉,根本走不到这里。
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.host == ViewerHost::Workbench {
            // Workbench 的关闭检查会回读当前 FileViewer 的 dirty 状态。当前按键
            // listener 仍持有本实体的 update 租约，直接回调会 double-lease。
            let source = self.source.clone();
            window.defer(cx, move |window, cx| {
                crate::workbench_area::close_document_source(source, window, cx);
            });
            return;
        }
        if self.draft(cx) == self.saved {
            close(window, cx);
            return;
        }
        Confirm::new(t("fileViewer", "unsavedTitle"), t("fileViewer", "unsavedMessage")).open(
            |window, cx| {
                // 确认框自己还压在栈顶,`close_guarded` 这时会拒绝动手
                // (它只关栈顶那一个)—— 排到本轮之后再关
                window.defer(cx, |window, cx| close(window, cx));
            },
            window,
            cx,
        );
    }

    /// 打开 / 换文件后把焦点放到该放的地方:能编辑就进编辑器,
    /// 否则留在容器上(Ctrl+S / Esc 挂在容器的 `on_key_down` 上,
    /// 焦点不在这条链上就收不到键)。
    fn can_take_async_focus(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match self.host {
            // Modal host itself is an active dialog. The overlay stack tells us
            // whether a nested confirm/menu has since moved above it.
            ViewerHost::Modal => crate::overlay::is_top(crate::overlay::key(kind::FILE_VIEWER)),
            ViewerHost::Workbench => {
                crate::workbench_area::is_document_active(&self.source, cx)
                    && !window.has_active_dialog(cx)
                    && crate::overlay::allows(crate::overlay::Yield::ToOverlay)
            }
        }
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
                    })
                    .when(self.host == ViewerHost::Modal, |el| {
                        el.child(
                            div()
                                .id("file-viewer-close")
                                .px(px(4.0))
                                .text_size(ui::font_px(16.0))
                                .text_color(ui::text_muted())
                                .cursor_pointer()
                                .hover(|el| el.text_color(ui::text_primary()))
                                .child("✕")
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.request_close(window, cx)
                                })),
                        )
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

    /// 自绘一行 md 图片(整行只有图片的那些行,见 [`parse_image_line`])。
    ///
    /// TextView 那条路把图片目标一律当**网络 URI**(见本模块「markdown 分段」
    /// 一节),于是 README 里 `![主界面](docs/screenshots/main.png)` 这种相对路径
    /// 在预览里什么都不出 —— 原版是 `convertFileSrc(fileDir + '/' + src)`。
    /// 这里按当前文件所在目录解析成 `Resource::Path` 自己画。
    ///
    /// 远程图片(徽章、外链截图)走 `Resource::Uri`,字节由 [`PreviewHttpClient`]
    /// 拉回来 —— gpui 默认的 `NullHttpClient`(`gpui/app.rs:2343`)压根发不出请求。
    ///
    /// 宽度自己算而不是甩给 `max_w`:gpui 的 `img` 在宽高都是 `Auto` 时把两轴
    /// 一起填成图片原尺寸(`elements/img.rs:337-363`),此后 `max_w` 只压得住宽、
    /// 高度还是原值,大图会被 `object_fit` 缩成一小条飘在大片留白里。**给定宽度
    /// 之后**高度那一支才会按比例算。
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
            let label = gpui::SharedString::from(if image.alt.is_empty() {
                file_name_of(&image.url).to_string()
            } else {
                image.alt.clone()
            });
            let source = resolve_image_src(&image.url, &base_dir);
            let el = if self.source.is_remote() {
                match source {
                    MdImageSrc::Remote(url) => {
                        self.render_md_remote_image(id, label, &url, each_w, window, cx)
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
            // 本地目标要走「弹窗内跳转」,而那条路整条不做(见模块注释偏差 1)
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
            Some(Ok(data)) => img(path.to_path_buf())
                .id(id)
                .object_fit(gpui::ObjectFit::Contain)
                .w(px(image_display_width(&data, is_svg_target(&hint), avail_w)))
                .into_any_element(),
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
            Some(Ok(data)) => img(uri)
                .id(id)
                .object_fit(gpui::ObjectFit::Contain)
                .w(px(image_display_width(&data, is_svg_target(url), avail_w)))
                .into_any_element(),
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
                        sanitize_remote_html_urls(&sanitize_remote_markdown_images(&text)).into()
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
                                sanitize_remote_html_urls(&sanitize_remote_markdown_images(cell))
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

    /// 正文可用宽度:弹窗宽是 viewport*0.9(见 [`open`]),减掉两侧 24px padding,
    /// 再夹到正文的 860px 上限。图片自绘要按它定尺寸。
    fn preview_avail_width(&self, window: &Window) -> f32 {
        (f32::from(window.viewport_size().width) - 48.0).clamp(80.0, 860.0)
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
        let avail_w = self.preview_avail_width(window);
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
                div().max_w(px(860.0)).mx_auto().w_full().children(
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
                                MdBlock::Images(images) => {
                                    self.render_md_images(ix, images, avail_w, window, cx)
                                }
                            };
                            div()
                                .when(*mt > 0.0, |el| el.mt(ui::font_px(*mt)))
                                .child(content)
                                .into_any_element()
                        })
                        .collect::<Vec<_>>(),
                ),
            )
            .into_any_element()
    }

    /// HTML 预览。**富文本简版渲染,不是浏览器** —— GPUI 侧没有 iframe 等价物,
    /// `TextView::html` 与 markdown 那支是同一个渲染器:标题 / 段落 / 列表 /
    /// 表格 / 图片 / 链接认得,CSS 与脚本一概不跑,带样式的页面会走样。
    ///
    /// 这正是当初「只留源码态」的理由(模块注释偏差 2)。现在改为提供,配套两条:
    /// 顶上一句说明写清楚它是简版,工具栏常驻「用浏览器打开」给真效果的出口。
    /// 图片与其它本地资源靠 [`rewrite_html_urls`] 转 `file://`(原版是
    /// `convertFileSrc`),由 [`PreviewHttpClient`] 读盘。
    fn render_html(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let source = if self.source.is_remote() {
            sanitize_remote_html_urls(self.preview_source())
        } else {
            rewrite_html_urls(self.preview_source(), &self.preview_base_dir())
        };
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
                    .max_w(px(860.0))
                    .mx_auto()
                    .w_full()
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
            // Ctrl/Cmd+S 与 Esc。挂在容器上而不是绑 action:
            // 绑成全局 action 要动 `main.rs` 的 bindings 表,而这两个键**只在本弹窗里**
            // 有意义;`on_key_down` 沿焦点链冒泡上来,焦点在编辑器里照样收得到
            // (gpui-component 的 code editor 不吃 Ctrl+S)。
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                let ks = &event.keystroke;
                let mods = &ks.modifiers;
                if this.host == ViewerHost::Modal && ks.key == "escape" && !mods.modified() {
                    cx.stop_propagation();
                    this.request_close(window, cx);
                    return;
                }
                if this.host == ViewerHost::Workbench
                    && ks.key == "w"
                    && mods.secondary()
                    && !mods.shift
                    && !mods.alt
                {
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
    fn 图片行_认得四种常见写法() {
        // 单张
        let imgs = parse_image_line("![主界面](docs/screenshots/main.png)").unwrap();
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].url, "docs/screenshots/main.png");
        assert_eq!(imgs[0].alt, "主界面");
        assert!(imgs[0].link.is_none());

        // 带 title
        let imgs = parse_image_line(r#"  ![图](a.png "标题")  "#).unwrap();
        assert_eq!(imgs[0].url, "a.png");
        assert_eq!(imgs[0].title.as_deref(), Some("标题"));

        // 链接包裹(徽章)
        let imgs = parse_image_line("[![CI](https://img.shields.io/x.svg)](https://ci.example)")
            .unwrap();
        assert_eq!(imgs[0].url, "https://img.shields.io/x.svg");
        assert_eq!(imgs[0].link.as_deref(), Some("https://ci.example"));

        // 一行并排两张
        let imgs = parse_image_line("![a](1.png) ![b](2.png)").unwrap();
        assert_eq!(imgs.len(), 2);
        assert_eq!(imgs[1].url, "2.png");

        // 尖括号写法(路径里有空格)
        let imgs = parse_image_line("![x](<my shots/a b.png>)").unwrap();
        assert_eq!(imgs[0].url, "my shots/a b.png");
    }

    #[test]
    fn 图片行_混了别的东西就整行放弃() {
        // 前后有文字 → 交给 TextView(内联图片不自绘)
        assert!(parse_image_line("看这张 ![a](1.png)").is_none());
        assert!(parse_image_line("![a](1.png) 就是主界面").is_none());
        // 列表项 / 引用块有前缀语法,拆出来会毁结构
        assert!(parse_image_line("- ![a](1.png)").is_none());
        assert!(parse_image_line("> ![a](1.png)").is_none());
        // 四空格缩进是代码块
        assert!(parse_image_line("    ![a](1.png)").is_none());
        // 空目标 / 只是链接不是图片
        assert!(parse_image_line("![a]()").is_none());
        assert!(parse_image_line("[文档](a.md)").is_none());
        assert!(parse_image_line("").is_none());
    }

    #[test]
    fn 图片行_从文本块里拆出来自绘() {
        let src = "# 标题\n\n上面一句说明\n![主界面](docs/main.png)\n下面一句\n\n结尾";
        let segs = split_md_blocks(src);
        // 标题 / 说明 / 图片 / 下面一句 / 结尾
        assert_eq!(segs.len(), 5, "{segs:?}");
        let MdSegment::Images(imgs) = &segs[2] else {
            panic!("第三段应是图片:{segs:?}");
        };
        assert_eq!(imgs[0].url, "docs/main.png");
        assert!(matches!(&segs[1], MdSegment::Text(t) if t == "上面一句说明"));
        assert!(matches!(&segs[3], MdSegment::Text(t) if t == "下面一句"));

        // 围栏代码块里的图片语法是代码,不拆
        let segs = split_md_blocks("```md\n![a](1.png)\n```");
        assert_eq!(segs.len(), 1, "{segs:?}");
        assert!(matches!(&segs[0], MdSegment::Text(_)));
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
    }

    #[test]
    fn 远程富文本只允许显式网络资源() {
        let double_tick = "``[code](file:///tmp/double)``";
        assert_eq!(
            markdown_code_span_len(&format!("{double_tick} tail")),
            Some(double_tick.len())
        );
        assert_eq!(markdown_code_span_len("```unmatched``"), None);

        let markdown = concat!(
            "- ![secret](file:///home/user/secret.png)\n",
            "![tracker](http://127.0.0.1:8080/a.png)\n",
            "`![code](file:///tmp/code.png)`\n",
            "```md\n![fenced](file:///tmp/fenced.png)\n```",
        );
        let sanitized = sanitize_remote_markdown_images(markdown);
        assert!(sanitized.contains("- [secret]"), "{sanitized}");
        assert!(
            sanitized.contains("![tracker](http://127.0.0.1:8080/a.png)"),
            "{sanitized}"
        );
        assert!(!sanitized.contains("file:///home/user/secret.png"));
        assert!(sanitized.contains("`![code](file:///tmp/code.png)`"));
        assert!(sanitized.contains("![fenced](file:///tmp/fenced.png)"));

        let references = sanitize_remote_markdown_images(concat!(
            "![secret][local]\n",
            "[local]: <file:///home/user/secret.png> \"title\"\n",
            "![web][remote]\n",
            "[remote]: https://example.com/image.png\n",
        ));
        assert!(!references.contains("file:///"), "{references}");
        assert!(
            references.contains("[local]: about:blank"),
            "{references}"
        );
        assert!(
            references.contains("[remote]: https://example.com/image.png"),
            "{references}"
        );

        let links = sanitize_remote_markdown_images(concat!(
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

        let html = concat!(
            r#"<img src="file:///home/user/secret.png">"#,
            r#"<img src="http://127.0.0.1:8080/a.png">"#,
            r#"<a href="file:///etc/passwd">local</a>"#,
            r#"<a href="https://example.com/docs">web</a>"#,
        );
        let sanitized = sanitize_remote_html_urls(html);
        assert!(!sanitized.contains("file:///"), "{sanitized}");
        assert!(
            sanitized.contains(r#"src="http://127.0.0.1:8080/a.png""#),
            "{sanitized}"
        );
        assert!(sanitized.contains(r#"src="about:blank""#), "{sanitized}");
        assert!(sanitized.contains(r##"href="#""##), "{sanitized}");
        assert!(
            sanitized.contains("https://example.com/docs"),
            "{sanitized}"
        );

        let unquoted = sanitize_remote_html_urls(concat!(
            r#"<img src=file:///etc/passwd>"#,
            r#"<img/src=file:///etc/group>"#,
            r#"<img alt="x"src=file:///etc/hosts>"#,
            r#"<img src=https://example.com/image.png>"#,
            r#"<a href=../secret.txt>local</a>"#,
        ));
        assert!(!unquoted.contains("file:///"), "{unquoted}");
        assert!(
            unquoted.contains("src=https://example.com/image.png"),
            "{unquoted}"
        );
        assert!(unquoted.contains("src=about:blank"), "{unquoted}");
        assert!(unquoted.contains("href=#"), "{unquoted}");

        let stray_text = sanitize_remote_html_urls(
            "plain href=\" without a closing quote\n<img src=file:///etc/shadow>",
        );
        assert!(!stray_text.contains("file:///"), "{stray_text}");
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
    fn 命中行定位的两道闸() {
        let text = "a\nb\nc\n";
        assert_eq!(highlight_target(Some(2), true, text), Some(2));
        assert_eq!(highlight_target(Some(3), true, text), Some(3));
        // 越界不动(原版 `highlightLine > doc.lines` 直接 return)
        assert_eq!(highlight_target(Some(9), true, text), None);
        assert_eq!(highlight_target(Some(0), true, text), None, "行号是 1-based");
        // 换了文件(预览器已开着又点了另一条结果)之后旧行号作废
        assert_eq!(highlight_target(Some(2), false, text), None);
        // 文件树那条路压根不给行号
        assert_eq!(highlight_target(None, true, text), None);
        // 空文件也算有第 1 行
        assert_eq!(highlight_target(Some(1), true, ""), Some(1));
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
