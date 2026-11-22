//! 进程级共享的阻塞 HTTP 客户端单例(`zed-reqwest` blocking)。
//!
//! 文件预览外链图片([`crate::file_viewer::http`])、远程通知([`crate::remote_notify`])、
//! 模型价格表([`crate::pricing`])与版本自检([`crate::update_check`])共用同一个
//! [`reqwest::blocking::Client`],共享连接池与 TLS 会话缓存;超时与请求头由各调用方
//! 按单次请求 `.timeout(...)` / `.header(...)` 各自指定。
//!
//! ⚠️ **整体阻塞**,所有调用一律放后台线程 / `cx.background_executor()`,绝不落主线程。
//! ⚠️ 客户端本身**不设**默认超时或默认 `User-Agent`:前者由各调用方按场景设(10s / 15s),
//!    后者避免改变无自定义头调用方(价格表 / 远程通知)的请求语义。

use std::sync::OnceLock;

/// 进程级共享阻塞客户端。首次初始化失败时向上返回错误(不缓存失败),
/// 供需要把初始化失败转成业务错误的调用方使用(远程通知 / 价格表 / 更新检查)。
///
/// reqwest 默认读取系统代理(Windows 注册表 / `HTTP_PROXY` / `HTTPS_PROXY`),
/// 与此前各调用方各自 `Client::builder().build()` 的行为一致。
pub(crate) fn try_client() -> Result<&'static reqwest::blocking::Client, reqwest::Error> {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let built = reqwest::blocking::Client::builder().build()?;
    Ok(CLIENT.get_or_init(|| built))
}

/// 进程级共享阻塞客户端的兜底入口:构建失败时退回 `Client::default()`。
/// 用于预览器等不把客户端构建失败单独暴露为 `reqwest::Error` 的路径。
pub(crate) fn client() -> &'static reqwest::blocking::Client {
    try_client().unwrap_or_else(|err| {
        tracing::warn!(
            "[http] 初始化共享客户端失败,回落默认实例: {}",
            err.without_url()
        );
        static FALLBACK: OnceLock<reqwest::blocking::Client> = OnceLock::new();
        FALLBACK.get_or_init(reqwest::blocking::Client::default)
    })
}
