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
//! 2. **HTML 只有源码态**:GPUI 侧没有 iframe 等价物,`TextView::html` 是富文本渲染器
//!    (无 CSS / 无 JS / 无相对资源),画出来的东西与浏览器差得远,比不提供更误导人。
//!    按规格 B.6.3 的建议选「只留源码编辑器」。
//! 3. **遮罩点击不关窗**:Dialog 的遮罩关闭无法拦截,而关闭要先过「未保存确认」——
//!    留着就等于给草稿开了一条静默丢弃的路。改为只能 ✕ / Esc 关。

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, ImageAssetLoader,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, Resource,
    StatefulInteractiveElement, Styled, Subscription, Task, WeakEntity, Window, div, img,
    prelude::FluentBuilder as _, px,
};
use gpui_component::ActiveTheme as _;
use gpui_component::input::{Input, InputEvent, InputState, Position};
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::tooltip::Tooltip;
use mt_project::fs::FileContentResult;
use mt_project::watch::FsWatcher;
use mt_ui::icons::FileIcon;

use crate::i18n::t;
use crate::overlay::kind;
use crate::prompt::{Confirm, close_guarded, open_guarded};
use crate::ui;

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

    let view = cx.new(|cx| FileViewer::new(project_root, path, highlight_line, window, cx));
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
    project_root: PathBuf,
    /// 外部传进来的那一个。`highlight_line` 只在 `current == origin` 时生效
    /// (`FileViewerModal.tsx:486`:跳走之后行号就失效了)。
    origin_path: PathBuf,
    current_path: PathBuf,
    highlight_line: Option<u32>,

    loading: bool,
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

    preview: bool,
    dirty: bool,
    saving: bool,
    save_error: Option<String>,
    ext_changed: bool,
    last_save_at: Option<Instant>,

    watcher: Arc<FsWatcher>,
    watched: Option<PathBuf>,

    focus: FocusHandle,
    _fs_task: Task<()>,
    _editor_sub: Option<Subscription>,
}

impl FileViewer {
    fn new(
        project_root: PathBuf,
        path: PathBuf,
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

        let mut this = Self {
            project_root,
            origin_path: path.clone(),
            current_path: path,
            highlight_line,
            loading: false,
            error: None,
            result: None,
            editor: None,
            saved: String::new(),
            disk: String::new(),
            preview_draft: None,
            line_ending: LineEnding::Lf,
            // 原版初值就是 true(Markdown / HTML 打开先看渲染稿)
            preview: true,
            dirty: false,
            saving: false,
            save_error: None,
            ext_changed: false,
            last_save_at: None,
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
        self.project_root = project_root;
        self.origin_path = path.clone();
        self.current_path = path;
        self.highlight_line = highlight_line;
        self.preview = true;
        self.reload(window, cx);
        self.focus_content(window, cx);
    }

    fn path_str(&self) -> String {
        self.current_path.to_string_lossy().to_string()
    }

    fn file_name(&self) -> String {
        let p = self.path_str();
        file_name_of(&p).to_string()
    }

    fn is_img(&self) -> bool {
        is_image_file(&self.path_str())
    }

    /// 「预览 / 源码」段控件的显示条件。
    ///
    /// 原版是 `(isMd || isHtml) && canEdit`(`FileViewerModal.tsx:355`)。
    /// **HTML 那一半在 GPUI 侧被摘掉**(见模块注释偏差 2):没有 iframe 等价物,
    /// 只留源码编辑器,于是也就没有第二态可切。下面那道 `is_html_file` 判定
    /// 在语义上是冗余的(`.html` 本来也不是 Markdown),留着是为了让这条偏差
    /// 在代码里有落点 —— 上游哪天给了 WebView 等价物,把它换成 `|| is_html` 即可。
    fn has_preview_toggle(&self) -> bool {
        let path = self.path_str();
        if is_html_file(&path) {
            return false;
        }
        is_markdown_file(&path) && can_edit(self.is_img(), self.result.as_ref())
    }

    // ── 读盘 ──────────────────────────────────────────────

    /// 读当前文件并重建编辑器。图片分支不读盘(原版 `if (!open || isImg) return`)。
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rewatch();
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

        let root = self.project_root.clone();
        let path = self.current_path.clone();
        cx.spawn_in(window, async move |this, cx| {
            // 读盘是阻塞的,**不能在主线程上跑**
            let probe = (root.clone(), path.clone());
            let outcome = cx
                .background_executor()
                .spawn(async move { mt_project::fs::read_file_content(&probe.0, &probe.1) })
                .await;
            let _ = this.update_in(cx, |view: &mut FileViewer, window, cx| {
                // 回来时可能已经换了文件 —— 只认还对得上号的那一次
                if view.current_path != path {
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

    /// 内容到位:落基线 + 建编辑器。
    ///
    /// 「编辑基线与内容一起落位」是原版注释里点名的一条(`FileViewerModal.tsx:224`)——
    /// 分两步会出现「内容已换、基线还是旧文件」的窗口,那一瞬间的脏态是错的。
    fn apply_content(&mut self, res: FileContentResult, window: &mut Window, cx: &mut Context<Self>) {
        self.line_ending = LineEnding::detect(&res.content);
        let text = normalize_to_lf(&res.content);
        self.saved = text.clone();
        self.disk = text.clone();
        self.dirty = false;
        self.ext_changed = false;
        self.preview_draft = None;
        self.save_error = None;

        if can_edit(false, Some(&res)) {
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
        }
        self.result = Some(res);
        // 原版编辑器每次都是带 `autoFocus` 重新挂载的(`preview` 态下才不抢焦点),
        // 这里在内容落位之后统一把焦点摆回该在的地方
        self.focus_content(window, cx);
        cx.notify();
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
        if self.is_img() || self.result.is_none() {
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
        if self.draft(cx) != self.saved {
            // 脏:挂提示条让用户自己决定
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
        let text = self.draft(cx);
        if self.saving || text == self.saved {
            return;
        }
        self.saving = true;
        self.save_error = None;
        cx.notify();

        let root = self.project_root.clone();
        let path = self.current_path.clone();
        // 写回磁盘前把行尾还原(见模块注释)
        let on_disk = restore_line_ending(&text, self.line_ending);
        cx.spawn(async move |this, cx| {
            let probe = (root, path.clone(), on_disk);
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    mt_project::fs::write_file_content(&probe.0, &probe.1, &probe.2)
                })
                .await;
            let _ = this.update(cx, |view: &mut FileViewer, cx| {
                view.saving = false;
                if view.current_path != path {
                    return;
                }
                match outcome {
                    Ok(()) => {
                        view.saved = text.clone();
                        view.disk = text.clone();
                        view.last_save_at = Some(Instant::now());
                        // 保存期间用户可能又敲了字:按**最新**草稿重新比对,
                        // 而不是直接置 false(原版 `setDirty(draftRef.current !== text)`)
                        view.dirty = view.draft(cx) != text;
                        view.ext_changed = false;
                    }
                    // 失败挂顶部红条,不弹窗
                    Err(err) => view.save_error = Some(format!("{err:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    // ── 关闭 ──────────────────────────────────────────────

    /// 两段式退出的第二段:有未保存修改先问一句(`FileViewerModal.tsx:153-164`)。
    ///
    /// 第一段(编辑器搜索面板开着时 Esc 只关面板)是 GPUI **结构性免费**的:
    /// gpui-component 的搜索面板把 `escape` 绑在自己的 `Input` 上下文里、
    /// `on_action_escape` 不 `cx.propagate()`(`input/search.rs:305-307`),
    /// 焦点在面板上时 Esc 被它吃掉,根本走不到这里。
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    fn focus_content(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.editor {
            Some(editor) if !(self.has_preview_toggle() && self.preview) => {
                editor.update(cx, |state, cx| state.focus(window, cx));
            }
            _ => self.focus.focus(window),
        }
    }

    fn open_with_default_app(&self, cx: &mut App) {
        let path = self.current_path.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(err) = mt_project::editor::open_path_with_default_app(&path) {
                    eprintln!("[file-viewer] 默认程序打开失败: {err:#}");
                }
            })
            .detach();
    }

    // ── 渲染 ──────────────────────────────────────────────

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let name = self.file_name();
        let path = self.path_str();
        let can_edit = can_edit(self.is_img(), self.result.as_ref());
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
                    .when(self.has_preview_toggle(), |el| {
                        el.child(self.render_preview_toggle(cx))
                    })
                    .child(
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
                    ),
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

    /// 顶部两条提示条:保存失败(红)、外部修改(黄)。
    fn render_banners(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .flex_none()
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
                                .cursor_pointer()
                                .hover(|el| el.text_color(ui::text_primary()))
                                .child(t("fileViewer", "reloadDiscard"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.ext_changed = false;
                                    this.reload(window, cx);
                                })),
                        ),
                )
            })
    }

    /// 居中一行字 + 一个「使用默认工具打开」按钮(二进制 / 过大 / 图片解不出来)。
    fn render_fallback(&self, id: &'static str, message: String, cx: &mut Context<Self>) -> impl IntoElement {
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
            .child(
                ui::primary_button(id, t("fileViewer", "openWithDefaultApp")).on_click(
                    cx.listener(|this, _: &ClickEvent, _window, cx| this.open_with_default_app(cx)),
                ),
            )
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

    /// Markdown 预览。样式对照 `src/styles.css:813-943` 的 `.md-preview`:
    /// 容器 `p-6 max-w-[860px] mx-auto`、段间距 1 rem、正文 1.08rem/1.7。
    ///
    /// 代码块高亮是**改善**(原版 `.md-preview pre code` 只设颜色不做高亮),
    /// 且与编辑器同一份 `highlight_theme`,两处颜色一致。
    fn render_markdown(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let source = self
            .preview_draft
            .clone()
            .unwrap_or_else(|| self.disk.clone());
        let style = TextViewStyle {
            highlight_theme: cx.theme().highlight_theme.clone(),
            is_dark: cx.theme().mode.is_dark(),
            ..Default::default()
        };
        div()
            .id("file-viewer-md")
            .size_full()
            .overflow_y_scroll()
            .p(px(24.0))
            .child(
                div().max_w(px(860.0)).mx_auto().child(
                    TextView::markdown("file-viewer-md-body", source, window, cx)
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
                    return self.render_markdown(window, cx);
                }
                match &self.editor {
                    Some(editor) => div()
                        .size_full()
                        .child(Input::new(editor).h_full().appearance(false).bordered(false))
                        .into_any_element(),
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
                if ks.key == "escape" && !mods.modified() {
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
