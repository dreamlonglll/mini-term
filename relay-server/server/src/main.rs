//! 中转服务器入口:环境变量配置端口后启动。
//! `RELAY_BIND`(默认 0.0.0.0)/ `RELAY_PORT`(默认 8080)。

use mt_relay_server::{app, RelayState};

#[tokio::main]
async fn main() {
    let bind = std::env::var("RELAY_BIND").unwrap_or_else(|_| "0.0.0.0".into());
    let port = std::env::var("RELAY_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);

    let listener = tokio::net::TcpListener::bind((bind.as_str(), port))
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind}:{port}: {e}"));
    eprintln!("[relay] listening on {bind}:{port} (protocol v{})", mt_relay_protocol::PROTOCOL_VERSION);

    axum::serve(listener, app(RelayState::new()))
        .await
        .expect("relay server crashed");
}
