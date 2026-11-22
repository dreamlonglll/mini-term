//! 进程级共享的异步 HTTP 客户端单例(`zed-reqwest` async)。
//!
//! 文件预览外链图片([`crate::file_viewer::http`])、远程通知([`crate::remote_notify`])、
//! 模型价格表([`crate::pricing`])与版本自检([`crate::update_check`])共用同一个
//! [`reqwest::Client`],共享连接池与 TLS 会话缓存;超时与请求头由各调用方按单次请求
//! `.timeout(...)` / `.header(...)` 各自指定。
//!
//! 异步网络 IO 统一经 [`run_async`] 派发到 `mt_remote` 自持的进程级共享 tokio 运行时
//! (`mt-async-io`),不再使用 `reqwest::blocking` 的独立隐藏运行时线程,也不在弱网时
//! 占住 GPUI 后台线程池;外层 future 取消时底层任务同步 abort。
//!
//! ⚠️ 客户端本身**不设**默认超时或默认 `User-Agent`:前者由各调用方按场景设(10s / 15s),
//!    后者避免改变无自定义头调用方(价格表 / 远程通知)的请求语义。

use std::sync::OnceLock;

/// 进程级共享异步客户端。首次初始化失败时向上返回错误(不缓存失败),
/// 供需要把初始化失败转成业务错误的调用方使用(远程通知 / 价格表 / 更新检查)。
///
/// reqwest 默认读取系统代理(Windows 注册表 / `HTTP_PROXY` / `HTTPS_PROXY`),
/// 与此前各调用方各自 `Client::builder().build()` 的行为一致。
///
/// ⚠️ 应在 [`run_async`] 内部调用,确保首次构造与网络请求均处于共享 tokio 运行时中。
pub(crate) fn try_client() -> Result<&'static reqwest::Client, reqwest::Error> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client);
    }
    let built = reqwest::Client::builder().build()?;
    Ok(CLIENT.get_or_init(|| built))
}

/// 进程级共享异步客户端的兜底入口:构建失败时退回 `Client::default()`。
/// 用于预览器等不把客户端构建失败单独暴露为 `reqwest::Error` 的路径。
pub(crate) fn client() -> &'static reqwest::Client {
    try_client().unwrap_or_else(|err| {
        tracing::warn!(
            "[http] 初始化共享客户端失败,回落默认实例: {}",
            err.without_url()
        );
        static FALLBACK: OnceLock<reqwest::Client> = OnceLock::new();
        FALLBACK.get_or_init(reqwest::Client::default)
    })
}

/// 在进程级共享 tokio 运行时(`mt-async-io`)上执行一段异步 HTTP future。
pub(crate) async fn run_async<F, T>(fut: F) -> Result<T, String>
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    mt_remote::run_on_shared_runtime(fut).await
}

#[cfg(test)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::*;

    #[test]
    fn 共享异步客户端可在共享运行时完成环回请求() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
                );
            }
        });

        let body = futures::executor::block_on(async move {
            run_async(async move {
                let resp = try_client()
                    .map_err(|e| e.to_string())?
                    .get(format!("http://127.0.0.1:{port}/ping"))
                    .timeout(Duration::from_secs(5))
                    .send()
                    .await
                    .map_err(|e| e.to_string())?;
                assert_eq!(resp.status().as_u16(), 200);
                resp.text().await.map_err(|e| e.to_string())
            })
            .await
            .expect("共享运行时调用应成功")
            .expect("HTTP GET 应成功")
        });
        assert_eq!(body, "hello");
        server.join().unwrap();
    }
}
