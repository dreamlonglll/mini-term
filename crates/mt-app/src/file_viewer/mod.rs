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
//! # Tab:同一手法的第二件套
//!
//! GPUI 的整形器把 `\t` 画成零宽,Tab 缩进的文件(Go / Makefile)在编辑器里整篇顶格
//! (issue #74)。读入时把 Tab 按制表位展成空格、写回时还原,规则与取舍全在
//! [`crate::tab_expansion`] 的模块注释;本模块只在三处接线,都在
//! [`DocumentSession`] 里:落基线时展开、[`DocumentSession::begin_save`] 还原、
//! 保存收尾时重算映射。顺序是**先归一行尾再展开 Tab**,写回时反过来。
//!
//! # 与原版的偏差(逐条,详见各处注释)
//!
//! 1. **Markdown 链接的「本地文件」一律作为页签打开**,不在页内换文件:原版是
//!    单个 modal 里换文件、靠 `←` 历史栈回退;GPUI 版文件本来就是页签,已开的
//!    切过去、没开的新开,历史栈由页签条承担。外链确认 / 文档内锚点滚动两条与
//!    原版一致(gpui-component 0.6 起 `TextView::on_link_click` 开了回调口,
//!    0.5.1 时代这三条整块做不了)。见 [`FileViewer::follow_link`]。
//! 2. **本地 HTML 是简版渲染,不是浏览器**:GPUI 侧没有 iframe 等价物,`TextView::html`
//!    与 markdown 那支是同一个富文本渲染器(无 CSS / 无 JS)。此处曾按规格 B.6.3
//!    的建议「只留源码编辑器」,**已翻案**(用户要求):现在给预览态,但配一条
//!    说明 + 工具栏常驻「用浏览器打开」——走样的排版有解释、真效果有出口,
//!    比对着一屏源码有用。相对资源不再是问题,见 [`html_urls::rewrite_html_urls`]。远程
//!    HTML 属于不可信输入，只走源码编辑器，不进入富文本 HTML 渲染器。
//!
//! # 模块结构
//!
//! 原先是一个五千行的 `file_viewer.rs`,按现成的缝拆成了目录(纯逻辑与渲染原样
//! 搬移;保存 / 外部改动 / 远程冲突的状态位另抽成了纯结构 [`DocumentSession`]):
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | 本文件 | [`FileViewer`] 实体:构造、读盘 / 保存 / 监听的 IO 接线、编辑器、工具栏与横幅、内容分派 |
//! | [`document`] | [`DocumentSession`] 读写状态机(状态转换表在该模块注释里)与行尾三件套 |
//! | [`file_kind`] | 文件类型判定、路径比对、扩展名 → 语言名 |
//! | [`preview`] | Markdown / HTML 预览的渲染(`impl FileViewer` 的另一半)与自绘表格、图片占位 |
//! | [`markdown`] | Markdown 纯逻辑:分块、图片落点、本地图片改写、链接处置、表格排版参数 |
//! | [`sanitize`] | 不可信 Markdown(远程文档 / AI 会话正文)清洗 |
//! | [`html_urls`] | 本地 HTML 的资源 URL 改写 |
//! | [`mermaid`] | Mermaid 图表后台渲染与资源释放 |
//! | [`images`] | 查看器图片资源、进程级持有账本、看图页签换代去抖 |
//! | [`http`] | 预览用的进程级 HTTP 客户端 [`PreviewHttpClient`] |
//!
//! 各纯逻辑模块的单测在同目录的 `*_tests.rs`,视图层的在 `tests.rs`。

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ListAlignment, ListState, ParentElement, Pixels, Render, Resource,
    ScrollHandle, StatefulInteractiveElement, Styled, Subscription, Task, Window, div, img,
    prelude::FluentBuilder as _, px,
};
use gpui_component::WindowExt as _;
use gpui_component::input::{Editor, EditorState, InputEvent, Position, Search, TabSize};
use mt_project::fs::FileContentResult;
use mt_project::watch::{FsWatcher, WATCH_READY_BUDGET, WatchReady};
use mt_ui::icons::FileIcon;
use mt_ui::tooltip::TooltipExt as _;

use crate::i18n::t;
use crate::image_lightbox::ImageLightbox;
use crate::tab_expansion::TAB_WIDTH;
use crate::ui;

mod document;
mod file_kind;
mod html_urls;
mod http;
mod images;
mod markdown;
mod mermaid;
mod preview;
mod sanitize;

// 行尾三件套的对外路径:`tab_expansion` 的单测与文档链接按 `crate::file_viewer::…`
// 引用。本模块自己已不直接用(读写口径都收进了 `DocumentSession`),所以非测试
// 构建里它们只剩这个出口
#[cfg_attr(not(test), allow(unused_imports))]
pub use document::{LineEnding, normalize_to_lf, restore_line_ending};
pub use file_kind::language_for;
pub use http::PreviewHttpClient;
pub use sanitize::sanitize_session_markdown;

use document::{
    DocumentSession, FsChange, LoadStart, RefreshLoaded, RemoteRefreshFailurePresentation,
    RemoteSave, SaveStart,
};
use file_kind::{
    file_name_of, is_html_file, is_image_file, is_markdown_file, same_path, should_wrap,
};
use images::{
    IMAGE_RELOAD_DEBOUNCE, ReloadDebounce, ViewerImage, ViewerImageHolds, evict_viewer_image,
    local_image_resource, release_viewer_images,
};
use mermaid::{MermaidKey, release_mermaid_assets};
use preview::MdCache;

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
        /// 装箱:连接快照三百多字节,不装箱整个枚举(连带每个本地页签)都按它的尺寸占位
        connection: Box<mt_config::SshConnection>,
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
pub fn branch_of(
    is_img: bool,
    loading: bool,
    has_error: bool,
    result: Option<&FileContentResult>,
) -> Branch {
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

// ─── 视图 ─────────────────────────────────────────────────────

/// 编辑器全文;没有编辑器时 `None`(草稿口径见 [`DocumentSession::draft`])。
/// 自由函数而不是方法:要在 `self.doc` 被可变借用时只借 `self.editor`。
fn editor_text(editor: Option<&Entity<EditorState>>, cx: &App) -> Option<String> {
    editor.map(|editor| editor.read(cx).value().to_string())
}

pub struct FileViewer {
    source: DocumentSource,
    project_root: PathBuf,
    current_path: PathBuf,
    highlight_line: Option<u32>,

    /// 文档的读写状态:载入代次、基线投影、脏态、保存、外部改动、远程基线与
    /// 冲突。状态位怎么翻全在 [`DocumentSession`] 里,视图只发起 IO、喂结果。
    doc: DocumentSession,
    /// 编辑器实体。**换文件 / 显式重载才重建** —— `set_value` 会清撤销栈,
    /// 「预览 ↔ 源码」来回切只是不画它,草稿与撤销栈都留着
    /// (原版 `className={preview ? 'hidden' : 'h-full'}`,只隐藏不卸载)。
    editor: Option<Entity<EditorState>>,
    /// 切到预览那一刻的草稿快照;`None` = 干净,预览直接用
    /// [`DocumentSession::disk`]。
    preview_draft: Option<String>,
    /// markdown 预览的分块缓存,见 [`MdCache`]。`RefCell` 是因为
    /// [`Self::render_markdown`] 只拿得到 `&self`(gpui 的 `Render::render`
    /// 之下全是不可变借用),而这份缓存要在渲染途中回填。
    md_cache: RefCell<Option<MdCache>>,
    /// markdown 预览的虚拟化列表状态(`gpui::list`,一块一项)。**必须住在实体上**
    /// (理由同 [`Self::preview_scroll`]):逻辑滚动位置(块序号 + 块内偏移)与
    /// 各块量得的高度都在它里面,「预览 ↔ 源码」来回切靠它保住进度。
    md_list: ListState,
    /// 上次与 [`Self::md_list`] 对齐时的 `(分块代次, 视口宽)`,两者任一变了都要
    /// 让列表重新量高,见 [`Self::sync_md_list`]。`Cell` 是因为对齐发生在
    /// `render_markdown` 的 `&self` 里。
    md_list_sync: std::cell::Cell<(u64, Pixels)>,
    /// 远程 Markdown 图片按文档、按 URL 记录用户明确批准。未命中时只能画
    /// 占位，绝不能把 URI 交给进程级图片加载器。
    approved_remote_images: HashSet<String>,
    /// 本页签向 gpui 资源系统要过的 mermaid 图表(见 [`mermaid::MermaidAsset`])。
    /// gpui 的资源缓存与图集纹理都是**进程级、不自动淘汰**的:一份 2× 栅格的图表动辄
    /// 几 MB,「改一笔源码 → 切预览」一轮就多一份,不收就是显存慢慢被吃光
    /// (见 GPU 性能档案的「尺寸悬崖」)。于是记下要过的 key,重切分块时把
    /// 已不在文档里的那些连缓存带纹理一起放掉([`Self::release_stale_mermaid`]),
    /// 页签关闭时全放(`on_release`)。`RefCell` 的理由同 [`Self::md_cache`]。
    mermaid_requested: RefCell<HashSet<MermaidKey>>,
    /// 本页签向 gpui 资源系统要过的图片([`ViewerImage`]:看图页签那一张,或 md
    /// 预览里的本地 / 网络图片)。理由同 [`Self::mermaid_requested`] —— 一张
    /// 2560×1440 的截图解出来 15 MB 内存、上传后再占同样大的显存,一直不放;
    /// 差别在图片可能被别的页签同时用着,放不放由进程级账本
    /// [`ViewerImageHolds`] 判。重切分块时放掉不在文档里的
    /// ([`Self::release_stale_images`]),页签关闭时全放(`on_release`)。
    images_requested: RefCell<HashSet<Resource>>,
    /// 看图页签「磁盘上的图变了」的去抖(见 [`Self::schedule_image_reload`])。
    image_reload: ReloadDebounce,
    _image_reload_task: Option<Task<()>>,
    /// 点开的 mermaid 图表放大浮层(见 [`crate::image_lightbox`])。整窗遮罩由
    /// 它自己 `deferred` 画,这里只持有实体、在根上 `child` 出来;关闭 = 丢实体。
    lightbox: Option<Entity<ImageLightbox>>,
    _lightbox_sub: Option<Subscription>,
    /// html 预览的滚动位置。**必须住在实体上**:裸
    /// `overflow_y_scroll()` 的偏移存在按帧回收的 element state 里,切去终端页
    /// 的那几帧预览不渲染、状态被回收,切回来就跳回顶部。「预览 ↔ 源码」来回切
    /// 也靠它保住进度(源码态的滚动住在 `InputState` 实体里,组件自己管;
    /// markdown 预览的住在 [`Self::md_list`] 里)。
    preview_scroll: ScrollHandle,

    preview: bool,

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
        let watcher = Arc::new(FsWatcher::with_label("file-viewer", move |change| {
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
            doc: DocumentSession::new(),
            editor: None,
            preview_draft: None,
            md_cache: RefCell::new(None),
            // overdraw 600px:视口上下各多量这么多,滚轮一格(3 行 ≈ 60px)乃至
            // 快甩都不会露白;再大只是白付视口外的布局钱。`measure_all` 让首帧
            // 把所有块量一遍,滚动条的长度与位置才是准的(默认只量渲染过的块,
            // 拇指会随着滚动逐渐变短、一路乱跳);代价是打开时多一帧的全量布局,
            // 与旧路径每一帧的开销相同
            md_list: ListState::new(0, ListAlignment::Top, px(600.0)).measure_all(),
            md_list_sync: std::cell::Cell::new((0, px(0.0))),
            approved_remote_images: HashSet::new(),
            mermaid_requested: RefCell::new(HashSet::new()),
            images_requested: RefCell::new(HashSet::new()),
            image_reload: ReloadDebounce::default(),
            _image_reload_task: None,
            lightbox: None,
            _lightbox_sub: None,
            preview_scroll: ScrollHandle::new(),
            // 文件树打开 Markdown / HTML 时默认看渲染稿；内容搜索带行号时切到
            // 源码，否则命中光标虽然已经定位，用户看到的仍是无法对应行号的预览。
            preview: highlight_line.is_none(),
            watcher,
            watched: None,
            focus: cx.focus_handle(),
            _fs_task: fs_task,
            _editor_sub: None,
        };
        // 页签关掉时把要过的 mermaid 图表与图片从进程级缓存与图集里放掉(理由见
        // 字段注释;图片还有别的页签在用的,由账本留着)
        cx.on_release(|this: &mut Self, cx: &mut App| {
            let keys: Vec<MermaidKey> = this.mermaid_requested.get_mut().drain().collect();
            release_mermaid_assets(&keys, cx, None);
            let images: Vec<Resource> = this.images_requested.get_mut().drain().collect();
            release_viewer_images(images, cx, None);
        })
        .detach();
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
        self.doc.is_dirty()
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
            && can_edit(self.renders_local_image(), self.doc.result())
    }

    // ── 读盘 ──────────────────────────────────────────────

    /// 读当前文件并重建编辑器。图片分支不读盘(原版 `if (!open || isImg) return`)。
    fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 保存中不重建(理由与状态翻转见 `DocumentSession::begin_load`)
        let Some(start) = self.doc.begin_load(self.is_img()) else {
            return;
        };
        let watch_ready = self.rewatch();
        self.editor = None;
        self._editor_sub = None;
        cx.notify();
        let LoadStart::Read { generation } = start else {
            return;
        };

        let path = self.current_path.clone();
        match self.source.clone() {
            DocumentSource::Local { project_root, .. } => {
                cx.spawn_in(window, async move |this, cx| {
                    // 读盘是阻塞的,**不能在主线程上跑**
                    let probe = (project_root, path.clone());
                    let outcome = cx
                        .background_executor()
                        .spawn(async move {
                            // 刚换了监听目录就先等它挂上(有预算)再读:读完之后的外部
                            // 改动一定有事件,与同步时代「先挂监听、再读盘」同一次序
                            if let Some(ready) = watch_ready {
                                ready.wait(WATCH_READY_BUDGET);
                            }
                            mt_project::fs::read_file_content(&probe.0, &probe.1)
                        })
                        .await;
                    let _ = this.update_in(cx, |view: &mut FileViewer, window, cx| {
                        if view.current_path != path || !view.doc.settle_load(generation) {
                            return;
                        }
                        match outcome {
                            Ok(res) => view.apply_content(res, window, cx),
                            Err(err) => {
                                view.doc.fail_load(format!("{err:#}"));
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
                            mt_remote::read_file_content(&connection, &project_root, &remote_path)
                        })
                        .await;
                    let _ = this.update_in(cx, |view: &mut FileViewer, window, cx| {
                        if view.current_path != path || !view.doc.settle_load(generation) {
                            return;
                        }
                        match outcome {
                            Ok(content) => {
                                view.apply_remote_content(content, window, cx);
                            }
                            Err(err) => {
                                view.doc.fail_load(err);
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
        let text = self.doc.apply_local_content(res);
        self.install_editor(text, window, cx);
    }

    fn apply_remote_content(
        &mut self,
        content: mt_remote::RemoteFileReadResult,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.validate_remote_source(cx);
        match self.doc.apply_remote_content(content) {
            Some(text) => self.install_editor(text, window, cx),
            None => cx.notify(),
        }
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
        let generation = self.doc.begin_remote_refresh();
        let path = self.current_path.clone();
        let remote_path = path.to_string_lossy().into_owned();
        cx.notify();

        cx.spawn_in(window, async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    mt_remote::read_file_content(&connection, &project_root, &remote_path)
                })
                .await;
            let _ = this.update_in(cx, |view: &mut FileViewer, window, cx| {
                if view.current_path != path || !view.doc.settle_remote_refresh(generation) {
                    return;
                }
                view.validate_remote_source(cx);
                if view.doc.remote_source_invalid() {
                    return;
                }
                let has_editor = view.editor.is_some();
                match outcome {
                    Ok(content) => {
                        // 内容没变只换基线、脏了挂冲突,都不动编辑器;变了才按新内容重建
                        if let RefreshLoaded::Replaced(Some(text)) =
                            view.doc.on_refresh_loaded(content, has_editor)
                        {
                            view.install_editor(text, window, cx);
                        }
                    }
                    Err(error) => {
                        if view.doc.on_refresh_failed(error, has_editor)
                            == RemoteRefreshFailurePresentation::Fatal
                            && view.can_take_async_focus(window, cx)
                        {
                            view.focus.focus(window, cx);
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 基线已由 [`DocumentSession`] 落好,`text` 是它给的编辑器投影(已归一成 `\n`、
    /// Tab 已展开):建编辑器、定位命中行、摆焦点。
    fn install_editor(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.preview_draft = None;

        if can_edit(self.is_img(), self.doc.result()) {
            let name = self.file_name();
            let lang = language_for(&name);
            let wrap = should_wrap(&name);
            let tab_indented = self.doc.indents_with_tabs();
            let editor = cx.new(|cx| {
                let state = EditorState::new(window, cx)
                    .language(lang)
                    .line_number(true)
                    .soft_wrap(wrap)
                    .default_value(text.clone());
                // Tab 缩进的文件:Tab 键一次缩 `TAB_WIDTH` 个空格,写回时正好折成一个 `\t`
                // (组件默认 2 个,折不回去会留下半档空格);其余文件维持组件默认
                if tab_indented {
                    state.tab_size(TabSize {
                        tab_size: TAB_WIDTH,
                        hard_tabs: false,
                    })
                } else {
                    state
                }
            });
            // 每次编辑都要重算脏态(原版 `onDocChange` → `setDirty(doc !== savedRef)`)
            let sub = cx.subscribe(&editor, |this: &mut FileViewer, editor, event, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = editor.read(cx).value().to_string();
                    this.doc.on_edit(&value);
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
                    || mt_remote::connection_fingerprint(current)
                        != mt_remote::connection_fingerprint(connection)
            });
        if self.doc.set_remote_source_invalid(invalid) {
            cx.notify();
        }
    }

    /// 页签重新激活时，干净的远程文档后台重读一次；内容未变时保留编辑器实体，
    /// 脏草稿只做连接身份检查，外部变化继续由保存前基线比较兜底。
    pub fn on_activated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.validate_remote_source(cx);
        if self
            .doc
            .should_refresh_on_activation(self.source.is_remote(), self.is_img())
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

    /// 当前草稿(编辑器全文,`\n` 行尾、Tab 已展开)。没有编辑器时就是磁盘内容的投影。
    fn draft(&self, cx: &App) -> String {
        self.doc.draft(editor_text(self.editor.as_ref(), cx))
    }

    // ── 监听外部修改 ──────────────────────────────────────

    /// 换文件时把监听挪到新文件的**父目录**上(notify 是目录级监听)。
    ///
    /// 每个页签是进程级监听单例的一个订阅者:与文件树同时监听同一目录时后端只注册
    /// 一次,各自按自己的项目根收事件(见 `mt_project::watch` 模块注释)。这里只登记
    /// 期望,校验与注册在单例的后台线程里做;新挂的目录返回回执,调用方读盘前先等它。
    /// 上次注册失败的(目录当时不可访问等),每次重载都重试一次 —— 与同步时代
    /// 「失败不记账、下次再试」同一口径。
    fn rewatch(&mut self) -> Option<WatchReady> {
        if self.source.is_remote() {
            if let Some(old) = self.watched.take() {
                self.watcher.unwatch(&old);
            }
            return None;
        }
        let dir = self.current_path.parent().map(|p| p.to_path_buf());
        if self.watched == dir
            && !dir
                .as_deref()
                .is_some_and(|dir| self.watcher.is_failed(dir))
        {
            return None;
        }
        if let Some(old) = self.watched.take() {
            self.watcher.unwatch(&old);
        }
        let dir = dir?;
        let project = self.project_root.to_string_lossy().to_string();
        let ready = self.watcher.watch(&dir, &project);
        self.watched = Some(dir);
        Some(ready)
    }

    /// 逐条对照 `FileViewerModal.tsx:275-283`。看图页签另走换代
    /// ([`Self::schedule_image_reload`]),原版那边图片改了是不跟的。
    fn on_fs_change(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if self.source.is_remote() || !same_path(&path.to_string_lossy(), &self.path_str()) {
            return;
        }
        if self.is_img() {
            self.schedule_image_reload(window, cx);
            return;
        }
        // 回声窗口 / 脏 / 保存中的判定在 `DocumentSession::on_fs_change`;草稿过了
        // 回声窗口才取
        let editor = self.editor.as_ref();
        match self
            .doc
            .on_fs_change(Instant::now(), || editor_text(editor, cx))
        {
            FsChange::Ignore => {}
            FsChange::Flagged => cx.notify(),
            FsChange::Reload => self.reload(window, cx),
        }
    }

    /// 看图页签对应的图在磁盘上被改写了:去抖 [`IMAGE_RELOAD_DEBOUNCE`] 后换代。
    ///
    /// 此前这里直接 return,缓存里那份永远是第一次读到的,图改了页签照旧显示旧图。
    /// 一次保存在 Windows 上是一串 modify 事件(分块写、改大小、改时间戳各报
    /// 一次),写到一半就读只会读到半截文件。每来一个事件就换掉旧任务(= 取消旧
    /// 计时)重新计时,到点时手上的票还是最新的才动手。万一还是读到半截(写入
    /// 拖得比去抖还长),解码失败也只是落到「使用默认工具打开」那页,不会崩;
    /// 写完那一下的事件照样再换代一次。
    fn schedule_image_reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ticket = self.image_reload.bump();
        self._image_reload_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(IMAGE_RELOAD_DEBOUNCE).await;
            let _ = this.update_in(cx, |view: &mut FileViewer, window, cx| {
                if view.image_reload.is_current(ticket) {
                    view.reload_image(window, cx);
                }
            });
        }));
    }

    /// 换代:旧图从资源缓存与图集里放掉(**不看持有账** —— 文件变了,谁手上的
    /// 都是旧图),本页签下一帧 `use_asset` 取不到就从盘上重读;别的页签若也挂着
    /// 这张图,同样取到新的。
    fn reload_image(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let resource = local_image_resource(&self.current_path);
        let latest = cx
            .default_global::<ViewerImageHolds>()
            .0
            .take_latest(&resource);
        // `update_in` 里当前窗口被摘出了 `App.windows`,必须递进去
        // (同 `release_mermaid_assets` 的注释)
        evict_viewer_image(&resource, latest, cx, Some(window));
        // 整窗重画而不只是 notify 自己:别的页签(比如引用这张图的 README 预览)
        // 上一帧的场景里还拿着旧图的图块,窗口不脏就可能被原样再呈现一遍
        // (理由见 `evict_viewer_image`)
        window.refresh();
    }

    // ── 保存 ──────────────────────────────────────────────

    /// `FileViewerModal.tsx:251-272`。干净或在保存中时**静默返回** ——
    /// Ctrl+S 是肌肉记忆,不该弹任何东西。
    fn save(&mut self, cx: &mut Context<Self>) {
        self.save_with_mode(false, cx);
    }

    fn save_with_mode(&mut self, force: bool, cx: &mut Context<Self>) {
        let text = self.draft(cx);
        if !self.doc.prepare_save(&text) {
            return;
        }
        self.validate_remote_source(cx);
        let Some(SaveStart {
            generation,
            on_disk,
        }) = self.doc.begin_save(&text)
        else {
            return;
        };
        cx.notify();

        let path = self.current_path.clone();
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
                        if view.current_path != path || !view.doc.settle_save(generation) {
                            return;
                        }
                        let editor = view.editor.as_ref();
                        view.doc.on_local_save_result(
                            text.clone(),
                            outcome.map_err(|err| format!("{err:#}")),
                            Instant::now(),
                            || editor_text(editor, cx),
                        );
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
                let Some(baseline) = self.doc.remote_baseline().cloned() else {
                    self.doc.abort_save_read_only();
                    cx.notify();
                    return;
                };
                let connection = {
                    let store_entity = crate::store::AppStore::global(cx);
                    let store = store_entity.read(cx);
                    store.remote_connection_of(&project_id)
                };
                let Some(connection) = connection else {
                    self.doc.abort_save_source_invalid();
                    cx.notify();
                    return;
                };
                let remote_path = path.to_string_lossy().into_owned();
                cx.spawn(async move |this, cx| {
                    let outcome = cx
                        .background_executor()
                        .spawn(async move {
                            mt_remote::save_file_content(
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
                        if view.current_path != path || !view.doc.settle_save(generation) {
                            return;
                        }
                        view.validate_remote_source(cx);
                        if view.doc.remote_source_invalid() {
                            return;
                        }
                        let editor = view.editor.as_ref();
                        view.doc.on_remote_save_result(
                            text.clone(),
                            outcome.map(RemoteSave::from),
                            Instant::now(),
                            || editor_text(editor, cx),
                        );
                        cx.notify();
                    });
                })
                .detach();
            }
        }
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
            _ => self.focus.focus(window, cx),
        }
    }

    /// Route the workspace Ctrl/Cmd+F action into this document. Preview pages
    /// first reveal source, then dispatch the editor's native search action once
    /// the Input node exists in the next rendered dispatch tree.
    pub fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.doc.loading()
            || self.doc.error().is_some()
            || !can_edit(self.is_img(), self.doc.result())
        {
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
        crate::fs_ops::open_external(crate::fs_ops::ExternalOpen::Browser, path, cx);
    }

    fn open_with_default_app(&self, cx: &mut App) {
        if self.source.is_remote() {
            return;
        }
        let path = self.current_path.clone();
        crate::fs_ops::open_external(crate::fs_ops::ExternalOpen::DefaultApp, path, cx);
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
            mt_remote::connection_fingerprint(connection),
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
        let can_edit =
            !self.doc.remote_source_invalid() && can_edit(self.is_img(), self.doc.result());
        let dirty = self.doc.is_dirty();
        let saving = self.doc.is_saving();

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
                                .tip(t("fileViewer", "unsaved")),
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
                                .on_click(
                                    cx.listener(|this, _: &ClickEvent, _window, cx| this.save(cx)),
                                )
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
                seg(
                    "file-viewer-preview",
                    t("fileViewer", "preview").to_string(),
                    preview,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    // 切到预览时拍一份草稿快照:预览渲染的是「正在编辑的内容」,
                    // 不是磁盘旧文;干净时置 None,直接用磁盘内容
                    let draft = this.draft(cx);
                    this.preview_draft = (draft != this.doc.saved()).then_some(draft);
                    this.preview = true;
                    cx.notify();
                })),
            )
            .child(
                seg(
                    "file-viewer-source",
                    t("fileViewer", "source").to_string(),
                    !preview,
                )
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
            .when(self.doc.remote_source_invalid(), |el| {
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
            .when_some(self.doc.save_error().map(str::to_owned), |el, err| {
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
            .when_some(self.doc.save_warning().map(str::to_owned), |el, warning| {
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
            .when_some(
                self.doc.refresh_warning().map(str::to_owned),
                |el, warning| {
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
                },
            )
            .when(
                self.doc.has_remote_conflict() && !self.doc.remote_source_invalid(),
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
                                        let Some(current) = this.doc.take_remote_conflict() else {
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
            .when(self.doc.ext_changed(), |el| {
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
                                .when(!self.doc.is_saving(), |el| {
                                    el.cursor_pointer()
                                        .hover(|el| el.text_color(ui::text_primary()))
                                })
                                .when(self.doc.is_saving(), |el| el.opacity(0.5))
                                .child(t("fileViewer", "reloadDiscard"))
                                .when(!self.doc.is_saving(), |el| {
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
            .when(remote && !self.doc.remote_source_invalid(), |el| {
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

    /// 取一张查看器图片([`ViewerImage`]):首次用到时登记持有 —— **先记后要**,
    /// 理由同 mermaid(`use_asset` 一调用,资源系统里就有了这个 key 的任务)——
    /// 取到了顺手把位图记进账本,放的时候靠它找图集纹理。
    fn use_viewer_image(
        &self,
        resource: &Resource,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<<ViewerImage as gpui::Asset>::Output> {
        if self.images_requested.borrow_mut().insert(resource.clone()) {
            cx.default_global::<ViewerImageHolds>()
                .0
                .acquire(resource.clone());
        }
        let result = window.use_asset::<ViewerImage>(resource, cx);
        if let Some(Ok(image)) = &result {
            cx.default_global::<ViewerImageHolds>()
                .0
                .note(resource, Arc::downgrade(image));
        }
        result
    }

    /// 图片分支。**位图与 svg 都走 [`gpui::ImageAssetLoader`]**
    /// ([`ViewerImage`] 原样转交给它)—— 那条路里 gpui 对 svg 做了 `swap_rgba_pa_to_bgra`
    /// (`elements/img.rs:698-703`),颜色与预乘 alpha 都是对的;
    /// `mt_ui::icons::vector` 注释里记的红蓝互换是**另一条路**
    /// (`Image::from_bytes(ImageFormat::Svg, …)` 走 `platform.rs` 的
    /// `to_image_data`,那里确实漏了交换)。
    ///
    /// 画的是**这里取到的那一份**,不是 `img(path)`:后者走 gpui 自己的
    /// `ImgResourceLoader`(另一个资源类型),同一张图会再解一遍、多占一份
    /// 内存与显存,而且那份谁也放不掉。
    ///
    /// 解不出来的格式(`image` crate 默认 feature 不含 avif 解码)不留白屏:
    /// 走 [`Self::render_fallback`] 给一个「使用默认工具打开」。
    fn render_image(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let resource = local_image_resource(&self.current_path);
        match self.use_viewer_image(&resource, window, cx) {
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
            Some(Ok(data)) => div()
                .size_full()
                .p(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                // Img 的 object_fit 默认就是 Contain,与原版 `object-contain` 同义
                .child(img(data).size_full())
                .into_any_element(),
        }
    }

    fn render_content(&self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        match branch_of(
            self.is_img(),
            self.doc.loading(),
            self.doc.error().is_some(),
            self.doc.result(),
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
                    self.doc.error().unwrap_or_default().to_string(),
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
                        let ts = wrap.text_style();
                        ts.font_family =
                            Some(ui::ui_font_family().unwrap_or_else(|| "Cascadia Code".into()));
                        ts.font_fallbacks = Some(gpui::FontFallbacks::from_fonts(vec![
                            "Cascadia Mono".into(),
                            "Consolas".into(),
                            "JetBrains Mono".into(),
                            "Microsoft YaHei".into(),
                            "Segoe UI Emoji".into(),
                        ]));
                        ts.font_size = Some(px(13.0).into());
                        ts.line_height = Some(gpui::relative(1.6));
                        wrap.child(
                            Editor::new(editor)
                                .h_full()
                                .appearance(false)
                                .bordered(false),
                        )
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
            // 着色统一由容器层承担**一层**(与终端区同口径,见 terminal_area 的
            // 「不刷底色」注释):工具栏/横幅/内容区都坐在这一层上,背景图皮肤下
            // bg_document 半透明,整页透出氛围图;内容区不再自己刷 bg_base,
            // 免得两层叠乘把图盖死
            .bg(ui::bg_document())
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
                    .child(self.render_content(window, cx)),
            )
            // 图片放大浮层:实体自己 `deferred` 到整窗之上,挂在哪一层都一样;
            // 挂根上是为了不随预览列表的行一起被回收
            .when_some(self.lightbox.clone(), |el, lightbox| el.child(lightbox))
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
mod tests;
