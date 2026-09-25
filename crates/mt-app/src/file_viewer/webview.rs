//! 本地 HTML 的真预览:系统 WebView(Windows = WebView2,macOS = WKWebView),经
//! gpui-wry 挂在主窗口上。CSS 与脚本照跑,效果与浏览器一致;Linux(wry 要
//! webkit2gtk)、WebView 建不起来(没装 WebView2 运行时等)或设了
//! `MT_DISABLE_HTML_WEBVIEW=1` 时,回落到 [`super::preview`] 里的富文本简版渲染。
//!
//! # 与 GPUI 画面的合成
//!
//! 挖洞 / 逐帧显隐 / 键盘焦点归还全在 [`crate::native_view`],这里只管「这一页」:
//! 建 WebView、喂内容、处置导航。
//!
//! # 内容怎么进 WebView:自定义协议,不是 `file://`
//!
//! 页面挂在 [`ORIGIN`] 下,路径 = 文件相对**项目根**的路径;请求由
//! [`plan_request`] 判定后从盘上读(文档本身用预览源码,未保存的草稿也看得到)。
//! 这样相对路径(`css/a.css`、`../img/b.png`)与站点根路径(`/assets/x.js`,按项目根
//! 解析)都走得通,编辑后重载也不必落盘。
//!
//! # 安全口径
//!
//! 页面里的脚本与浏览器里一样照跑,但**同源**这件事比浏览器宽:浏览器打开
//! `file://` 时页面读不到别的本地文件,而这里整个项目都在同一个源下。于是
//! [`plan_request`] 收紧成「只给页面资源」:
//!
//! - 只放行网页资源的扩展名([`mime_for`] 的白名单)——`.env`、`config.json`、源码
//!   一律 403,`fetch` 读不到、`<iframe>` 也嵌不进来;
//! - `Sec-Fetch-Dest: empty`(`fetch` / XHR)一律 403,与浏览器 `file://` 下「本地
//!   页面 fetch 不到本地文件」同一口径;
//! - 只认项目根之内(规范化后比前缀,符号链接指到外面同样拒绝);
//! - 响应带 `nosniff`,扩展名骗不过 MIME 检查。
//!
//! 外链不在 WebView 里开:导航与新窗口请求一律拦下,交给与 Markdown 预览同一套
//! 处置(外链弹确认后交系统浏览器,本地文件作为页签打开);下载一律拒绝。

// Linux 上没有 WebView 分支,下面的纯逻辑只剩单测在用
#![cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]

use std::path::{Path, PathBuf};

/// 自定义协议名。
const SCHEME: &str = "mtpreview";

/// 预览页的源。WebView2 只拦得住 http(s),wry 在 Windows 上把自定义协议映射成
/// `http://<scheme>.localhost`;macOS 的 WKWebView 是真正的 `<scheme>://`。
#[cfg(not(target_os = "macos"))]
pub(super) const ORIGIN: &str = "http://mtpreview.localhost";
#[cfg(target_os = "macos")]
pub(super) const ORIGIN: &str = "mtpreview://localhost";

/// 设了 `MT_DISABLE_HTML_WEBVIEW=1` 就不建 WebView,一律走简版渲染(排障开关)。
pub(super) fn disabled() -> bool {
    std::env::var_os("MT_DISABLE_HTML_WEBVIEW").is_some_and(|v| v == "1")
}

/// URL 路径段编码:保留 RFC 3986 的 unreserved 字符与 `/`,其余按 UTF-8 字节 `%XX`。
pub(super) fn encode_path(rel: &str) -> String {
    let mut out = String::with_capacity(rel.len());
    for byte in rel.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `%XX` 解码。坏的转义原样保留;解出来不是 UTF-8 返回 `None`。
pub(super) fn decode_path(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 3 <= bytes.len()
            && let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// 文件相对项目根的路径(正斜杠)。不在根之内返回 `None`。
pub(super) fn rel_of(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<String> = rel
        .components()
        .map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Option<_>>()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// 相对路径 → 预览页 URL。
pub(super) fn page_url(rel: &str) -> String {
    format!("{ORIGIN}/{}", encode_path(rel))
}

/// 同源 URL → 相对项目根的路径(去掉查询串与片段、解码)。别的源返回 `None`。
pub(super) fn same_origin_rel(url: &str) -> Option<String> {
    let rest = url.strip_prefix(ORIGIN)?;
    let rest = rest
        .strip_prefix('/')
        .or_else(|| rest.is_empty().then_some(""))?;
    let path = rest.split(['?', '#']).next().unwrap_or("");
    decode_path(path)
}

/// 放行的网页资源及其 MIME。不在表里的扩展名一律不给页面读(见模块注释「安全口径」)。
pub(super) fn mime_for(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" | "cjs" => "text/javascript",
        "map" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "ogg" | "oga" => "audio/ogg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "vtt" => "text/vtt",
        _ => return None,
    })
}

/// 顶层导航放不放行:同源的 HTML 页照常在 WebView 里跳(本地页面互链),
/// `about:blank` 放行,其余一律拦下交给宿主处置。
pub(super) fn navigation_allowed(url: &str) -> bool {
    if url == "about:blank" {
        return true;
    }
    same_origin_rel(url).is_some_and(|rel| {
        rel.rsplit('/')
            .next()
            .and_then(mime_for)
            .is_some_and(|mime| mime == "text/html")
    })
}

/// 一次资源请求的处置。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Plan {
    /// 文档本身:回预览源码(草稿也算)。
    Doc,
    /// 盘上的文件(还没规范化 —— 读之前要再验一次在不在根里)。
    File(PathBuf),
    /// 拒绝,带 HTTP 状态码。
    Deny(u16),
}

/// 判定一次资源请求。`uri_path` 是请求 URL 的路径部分(未解码),`fetch_dest` 是
/// `Sec-Fetch-Dest` 请求头(平台不给就是 `None`)。
pub(super) fn plan_request(
    method: &str,
    uri_path: &str,
    fetch_dest: Option<&str>,
    doc_rel: &str,
    root: &Path,
) -> Plan {
    if method != "GET" && method != "HEAD" {
        return Plan::Deny(405);
    }
    let Some(decoded) = decode_path(uri_path.trim_start_matches('/')) else {
        return Plan::Deny(400);
    };
    let segments: Vec<&str> = decoded.split('/').filter(|s| !s.is_empty()).collect();
    // `..` / `.` 浏览器已经规范化掉了,再来就是手搓的请求;`:` 与 `\` 会让
    // `Path::join` 换盘符或跨段(`C:` 拼上去整个根就被替换了)
    if segments.is_empty()
        || segments
            .iter()
            .any(|s| *s == ".." || *s == "." || s.contains([':', '\\']))
    {
        return Plan::Deny(400);
    }
    let rel = segments.join("/");
    if rel == doc_rel {
        return Plan::Doc;
    }
    if fetch_dest == Some("empty") {
        return Plan::Deny(403);
    }
    if mime_for(segments[segments.len() - 1]).is_none() {
        return Plan::Deny(403);
    }
    let mut path = root.to_path_buf();
    for segment in segments {
        path.push(segment);
    }
    Plan::File(path)
}

#[cfg(any(windows, target_os = "macos"))]
pub(super) use imp::HtmlWebView;

/// 本页签的 WebView 走到哪一步了。
pub(super) enum HtmlView {
    /// 还没建。
    Idle,
    /// 建的任务已经排上(建 WebView2 要跑一段嵌套消息循环,不在渲染途中做)。
    Creating,
    #[cfg(any(windows, target_os = "macos"))]
    Ready(std::rc::Rc<HtmlWebView>),
    /// 建不起来(原因已进日志),本页签此后一直走简版渲染。
    Failed,
}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::{Arc, Mutex, OnceLock, mpsc};

    use futures::StreamExt as _;
    use futures::channel::mpsc::{UnboundedSender, unbounded};
    use gpui::{
        AnyElement, Context, IntoElement, ParentElement as _, Styled as _, Task, Window, div,
    };
    use wry::RequestAsyncResponder;
    use wry::http::{Request, Response};

    use super::{HtmlView, Plan, SCHEME, disabled, navigation_allowed, page_url, plan_request};
    use crate::file_viewer::{DocumentSource, FileViewer};
    use crate::i18n::t;
    use crate::native_view::{Hosted, Slot};
    use crate::ui;

    /// 资源服务要读的那几样。`Mutex` 是因为读盘在后台线程上完成。
    struct ServeState {
        root: PathBuf,
        /// 规范化后的项目根(读盘前比前缀用)。规范化失败 = 一个文件都不给。
        root_canonical: Option<PathBuf>,
        doc_rel: String,
        /// 文档本身的内容:预览源码(草稿或磁盘现内容)。
        doc_body: Arc<str>,
    }

    pub(in crate::file_viewer) struct HtmlWebView {
        pub(in crate::file_viewer) hosted: Rc<Hosted>,
        serve: Arc<Mutex<ServeState>>,
        doc_url: String,
        _links: Task<()>,
    }

    thread_local! {
        /// 全进程一份 WebView 上下文:WebView2 的用户数据目录落在数据目录下的
        /// `webview/`。不设的话 WebView2 默认往 exe 旁边建 `mini-term.exe.WebView2`,
        /// 装在 Program Files 里时没有写权限,WebView 直接建不起来。
        static WEB_CONTEXT: RefCell<Option<wry::WebContext>> = const { RefCell::new(None) };
    }

    impl HtmlWebView {
        fn new(
            root: &Path,
            doc: &Path,
            body: &str,
            links: UnboundedSender<String>,
            links_task: Task<()>,
            window: &mut Window,
            cx: &mut gpui::App,
        ) -> Result<Self, String> {
            let doc_rel = super::rel_of(root, doc)
                .ok_or_else(|| format!("{} 不在项目目录 {} 之内", doc.display(), root.display()))?;
            let serve = Arc::new(Mutex::new(ServeState {
                root: root.to_path_buf(),
                root_canonical: std::fs::canonicalize(root).ok(),
                doc_rel: doc_rel.clone(),
                doc_body: Arc::from(body),
            }));
            let doc_url = page_url(&doc_rel);

            let webview = WEB_CONTEXT.with(|context| {
                let mut context = context.borrow_mut();
                let context = context.get_or_insert_with(|| {
                    let dir = mt_config::active_data_dir()
                        .ok()
                        .map(|dir| dir.join("webview"));
                    wry::WebContext::new(dir)
                });
                let protocol_state = serve.clone();
                let navigation_links = links.clone();
                let window_links = links;
                wry::WebViewBuilder::new_with_web_context(context)
                    .with_visible(false)
                    .with_focused(false)
                    .with_devtools(cfg!(debug_assertions))
                    .with_url(&doc_url)
                    .with_asynchronous_custom_protocol(
                        SCHEME.to_string(),
                        move |_id, request, responder| {
                            handle_request(&protocol_state, request, responder)
                        },
                    )
                    .with_navigation_handler(move |url| {
                        if navigation_allowed(&url) {
                            return true;
                        }
                        let _ = navigation_links.unbounded_send(url);
                        false
                    })
                    // Windows 上这个回调跑在别的线程上(wry 为防死锁),只转发不处置
                    .with_new_window_req_handler(move |url, _features| {
                        let _ = window_links.unbounded_send(url);
                        wry::NewWindowResponse::Deny
                    })
                    .with_download_started_handler(|_, _| false)
                    .build_as_child(window)
                    .map_err(|err| err.to_string())
            })?;
            Ok(Self {
                hosted: Hosted::register(webview, window, cx),
                serve,
                doc_url,
                _links: links_task,
            })
        }

        /// 预览源码变了(保存、外部改动、切预览时的草稿快照)就重新载入文档。
        /// 每帧都调,没变时只是一次字符串比较。
        fn sync(&self, body: &str) {
            let changed = {
                let mut state = self.serve.lock().unwrap_or_else(|e| e.into_inner());
                if *state.doc_body != *body {
                    state.doc_body = Arc::from(body);
                    true
                } else {
                    false
                }
            };
            if changed {
                let _ = self.hosted.webview().load_url(&self.doc_url);
            }
        }
    }

    /// 自定义协议的入口(UI 线程)。判定在这里做,读盘丢给后台线程,
    /// wry 负责把应答送回 UI 线程。
    fn handle_request(
        state: &Arc<Mutex<ServeState>>,
        request: Request<Vec<u8>>,
        responder: RequestAsyncResponder,
    ) {
        let state = state.lock().unwrap_or_else(|e| e.into_inner());
        let dest = request
            .headers()
            .get("sec-fetch-dest")
            .and_then(|value| value.to_str().ok());
        match plan_request(
            request.method().as_str(),
            request.uri().path(),
            dest,
            &state.doc_rel,
            &state.root,
        ) {
            Plan::Doc => {
                let body = state.doc_body.as_bytes().to_vec();
                responder.respond(response(200, Some("text/html; charset=utf-8"), body));
            }
            Plan::File(path) => {
                let root = state.root_canonical.clone();
                drop(state);
                run_in_background(move || responder.respond(read_file(&path, root.as_deref())));
            }
            Plan::Deny(status) => responder.respond(response(status, None, Vec::new())),
        }
    }

    fn read_file(path: &Path, root: Option<&Path>) -> Response<Vec<u8>> {
        let Some(root) = root else {
            return response(403, None, Vec::new());
        };
        let Ok(real) = std::fs::canonicalize(path) else {
            return response(404, None, Vec::new());
        };
        if !real.starts_with(root) {
            return response(403, None, Vec::new());
        }
        let mime = real
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(super::mime_for);
        match std::fs::read(&real) {
            Ok(bytes) => response(200, mime, bytes),
            Err(_) => response(404, None, Vec::new()),
        }
    }

    fn response(status: u16, mime: Option<&str>, body: Vec<u8>) -> Response<Vec<u8>> {
        let mut builder = Response::builder()
            .status(status)
            .header("Cache-Control", "no-store")
            .header("X-Content-Type-Options", "nosniff");
        if let Some(mime) = mime {
            builder = builder.header("Content-Type", mime);
        }
        builder
            .body(body)
            .unwrap_or_else(|_| Response::new(Vec::new()))
    }

    /// 读盘用的一条常驻后台线程。页面资源多是小文件,一条足够;不用 gpui 的
    /// background executor 是因为协议回调里拿不到 `App`。
    fn run_in_background(job: impl FnOnce() + Send + 'static) {
        type Job = Box<dyn FnOnce() + Send>;
        static QUEUE: OnceLock<Option<mpsc::Sender<Job>>> = OnceLock::new();
        let queue = QUEUE.get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("html-preview-io".into())
                .spawn(move || {
                    for job in rx {
                        job();
                    }
                })
                .ok()
                .map(|_| tx)
        });
        match queue {
            Some(tx) => {
                let _ = tx.send(Box::new(job));
            }
            None => job(),
        }
    }

    impl FileViewer {
        /// HTML 预览的 WebView 分支。返回 `None` = 走简版渲染。
        pub(in crate::file_viewer) fn render_html_webview(
            &self,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Option<AnyElement> {
            if disabled() || self.source.is_remote() {
                return None;
            }
            let ready = match &*self.html_view.borrow() {
                HtmlView::Failed => return None,
                HtmlView::Ready(view) => Some(view.clone()),
                HtmlView::Idle | HtmlView::Creating => None,
            };
            if let Some(view) = ready {
                view.sync(self.preview_source());
                return Some(
                    div()
                        .size_full()
                        .child(Slot::new(view.hosted.clone()))
                        .into_any_element(),
                );
            }
            let idle = matches!(*self.html_view.borrow(), HtmlView::Idle);
            if idle {
                *self.html_view.borrow_mut() = HtmlView::Creating;
                cx.spawn_in(window, async move |this, cx| {
                    let _ = this.update_in(cx, |view: &mut FileViewer, window, cx| {
                        view.create_html_view(window, cx)
                    });
                })
                .detach();
            }
            Some(
                self.render_center(t("fileViewer", "loading").to_string(), ui::text_muted())
                    .into_any_element(),
            )
        }

        fn create_html_view(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            let DocumentSource::Local { project_root, .. } = &self.source else {
                *self.html_view.borrow_mut() = HtmlView::Failed;
                return;
            };
            let (tx, mut rx) = unbounded::<String>();
            let links_task = cx.spawn_in(window, async move |this, cx| {
                while let Some(url) = rx.next().await {
                    if this
                        .update_in(cx, |view: &mut FileViewer, window, cx| {
                            view.on_webview_link(&url, window, cx)
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            });
            let root = project_root.clone();
            let doc = self.current_path.clone();
            let body = self.preview_source().to_string();
            let state = match HtmlWebView::new(&root, &doc, &body, tx, links_task, window, cx) {
                Ok(view) => HtmlView::Ready(Rc::new(view)),
                Err(err) => {
                    eprintln!("[html-preview] WebView 建立失败,改用简版渲染: {err}");
                    HtmlView::Failed
                }
            };
            *self.html_view.borrow_mut() = state;
            cx.notify();
        }

        /// WebView 里被拦下的导航 / 新窗口请求。同源的本地文件作为页签打开,
        /// 其余交给 Markdown 预览那套链接处置(外链弹确认、mailto 这类交系统)。
        fn on_webview_link(&mut self, url: &str, window: &mut Window, cx: &mut Context<Self>) {
            let href = match super::same_origin_rel(url) {
                Some(rel) => {
                    let DocumentSource::Local { project_root, .. } = &self.source else {
                        return;
                    };
                    let mut path = project_root.clone();
                    path.extend(rel.split('/').filter(|s| !s.is_empty()));
                    // follow_link 会再解一遍 %XX、按 `#` / `?` 截断,这三个字符先转义
                    path.to_string_lossy()
                        .replace('\\', "/")
                        .replace('%', "%25")
                        .replace('#', "%23")
                        .replace('?', "%3F")
                }
                None => {
                    let lower = url.to_ascii_lowercase();
                    let handled = ["http://", "https://", "mailto:", "tel:"];
                    if !handled.iter().any(|prefix| lower.starts_with(prefix)) {
                        return;
                    }
                    url.to_string()
                }
            };
            self.follow_link(&href, window, cx);
        }
    }
}

#[cfg(test)]
#[path = "webview_tests.rs"]
mod tests;
