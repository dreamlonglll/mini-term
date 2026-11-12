//! 预览用的进程级 HTTP 客户端:富文本渲染器里的图片 URI(本地 `file://` 与网络
//! `http(s)://`)都经它取字节。[`PreviewHttpClient`] 经 `file_viewer` 再导出给
//! `main` 装配。

use std::path::Path;
use std::time::Duration;

use anyhow::Context as _;
use futures::future::BoxFuture;
use gpui::http_client::{
    AsyncBody, HttpClient, Request, Response, StatusCode, Url, http::HeaderValue,
};

/// 预览器的 HTTP 客户端,装在 `main` 里(`cx.set_http_client`)。
///
/// gpui 默认那份是 `NullHttpClient`(`gpui/app.rs:2343`,`send()` 直接报错),
/// 而 gpui-component 的富文本渲染器把图片一律画成 `img(SharedUri)`
/// (`text/node.rs:609`)—— URI 走的就是 http client。于是预览里的图片全靠这条路:
///
/// - `file://`:本地图片。md / html 源里的相对路径在渲染前被改写成绝对 file URL
///   ([`super::markdown::rewrite_md_image_urls`] /
///   [`super::html_urls::rewrite_html_urls`]),到这里读盘返回。
/// - `http(s)://`:网络图片(README 顶上的徽章、外链截图)。`reqwest::blocking`
///   拉回来 —— 与价格表那条链同一个客户端库(见 `pricing::fetch_models_dev`)。
///
/// 其余 scheme 一律拒绝。本地资源只读普通文件并限制 32MB；出网资源另有 10s
/// 超时(`reqwest::blocking` 默认无限等)，同样限制 32MB。详见
/// [`fetch_local_preview_bytes`] / [`fetch_remote_bytes`]。
///
/// 这是进程级客户端，不只服务文件页；其它富文本入口必须先走对应安全策略。
/// AI 会话正文统一经 [`super::sanitize_session_markdown`] 禁掉全部自动资源请求。
pub struct PreviewHttpClient;

const PREVIEW_IMAGE_MAX_BYTES: u64 = 32 * 1024 * 1024;

impl HttpClient for PreviewHttpClient {
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
        mt_core::path::canonicalize(path).with_context(|| format!("读不到 {}", path.display()))?;
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

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
