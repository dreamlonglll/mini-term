//! 中转服务器入口:环境变量配置后启动。
//! - `RELAY_BIND`(默认 0.0.0.0)/ `RELAY_PORT`(默认 8080)
//! - `RELAY_PWA_DIR`(默认 ./pwa):移动端 PWA 静态资源目录

use mt_relay_server::{app_with_pwa, RelayState};

#[tokio::main]
async fn main() {
    let bind = std::env::var("RELAY_BIND").unwrap_or_else(|_| "0.0.0.0".into());
    let port = std::env::var("RELAY_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    let pwa_dir = std::env::var("RELAY_PWA_DIR").unwrap_or_else(|_| "./pwa".into());

    let listener = tokio::net::TcpListener::bind((bind.as_str(), port))
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind}:{port}: {e}"));
    eprintln!(
        "[relay] listening on {bind}:{port} (protocol v{}, pwa dir: {pwa_dir})",
        mt_relay_protocol::PROTOCOL_VERSION
    );

    axum::serve(listener, app_with_pwa(RelayState::new(), &pwa_dir))
        .await
        .expect("relay server crashed");
}
