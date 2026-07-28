//! 中转服务器入口:环境变量配置后启动。
//! - `RELAY_BIND`(默认 0.0.0.0)/ `RELAY_PORT`(默认 8080)
//! - `RELAY_PWA_DIR`(默认 ./pwa):移动端 PWA 静态资源目录
//! - `MT_RELAY_DESKTOP_KEY`(**必配**):桌面端接入共享密钥。未配置时 fail-closed,
//!   拒绝一切桌面端连接——"能跑起来"不等于"配好了"(见 ADR 0002)。

use mt_relay_server::{app_with_pwa, RelayState};

#[tokio::main]
async fn main() {
    let bind = std::env::var("RELAY_BIND").unwrap_or_else(|_| "0.0.0.0".into());
    let port = std::env::var("RELAY_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    let pwa_dir = std::env::var("RELAY_PWA_DIR").unwrap_or_else(|_| "./pwa".into());
    let state = RelayState::new().with_desktop_key(std::env::var("MT_RELAY_DESKTOP_KEY").ok());

    let listener = tokio::net::TcpListener::bind((bind.as_str(), port))
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind}:{port}: {e}"));
    // 打实际绑定端口(RELAY_PORT=0 时为系统分配的临时端口,测试据此定位)
    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    eprintln!(
        "[relay] listening on {bind}:{actual_port} (protocol v{}, pwa dir: {pwa_dir})",
        mt_relay_protocol::PROTOCOL_VERSION
    );
    if state.desktop_key_configured() {
        eprintln!("[relay] desktop key configured: desktop connections require MT_RELAY_DESKTOP_KEY");
    } else {
        eprintln!(
            "[relay] MT_RELAY_DESKTOP_KEY is NOT set — ALL desktop connections will be rejected. \
             Set it on the relay and enter the same value in mini-term's Mobile panel."
        );
    }

    axum::serve(listener, app_with_pwa(state, &pwa_dir))
        .await
        .expect("relay server crashed");
}
