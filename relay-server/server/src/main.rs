//! 中转服务器入口:环境变量配置后启动。
//! - `RELAY_BIND`(默认 0.0.0.0)/ `RELAY_PORT`(默认 8080)
//! - `RELAY_PWA_DIR`(默认 ./pwa):移动端 PWA 静态资源目录
//! - `MT_RELAY_DESKTOP_KEY`(**必配**):桌面端接入共享密钥。未配置时 fail-closed,
//!   拒绝一切桌面端连接——"能跑起来"不等于"配好了"(见 ADR 0002)。
//! - `RELAY_MAX_CONNECTIONS`(默认 64)/ `RELAY_MAX_CONNECTIONS_PER_IP`(默认 16):
//!   并发 WebSocket 连接数的全局 / 单客户端上限,超限在升级前回 503 / 429。
//! - `RELAY_MAX_DESKTOPS`(默认 64):同时活跃/已配对的桌面端房间数上限。
//! - `RELAY_CLIENT_IP_HEADER`(默认不设 = 用 TCP 对端地址):按 IP 计数时取客户端地址
//!   的请求头。**反代后面必须配**(Cloudflare 橙云填 `CF-Connecting-IP`),否则所有
//!   连接的对端都是反代自己;没有反代改写该头时**不要**配(客户端可自报任意值)。

use std::io::IsTerminal as _;
use std::net::SocketAddr;
use std::str::FromStr as _;

use axum::http::HeaderName;
use mt_relay_server::{app_with_pwa, RelayLimits, RelayState};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

fn init_tracing() {
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|raw| Targets::from_str(raw.trim()).ok())
        .unwrap_or_else(|| {
            Targets::new()
                .with_default(LevelFilter::WARN)
                .with_target("mt_relay_server", LevelFilter::INFO)
        });
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(std::io::stderr().is_terminal())
                .compact(),
        )
        .try_init();
}

/// 读一个正整数环境变量;没设 / 空串 / 解析不出 / 为 0 都回落默认值(后两种打一行提示)。
/// 空串按没设处理:docker-compose 里 `"${VAR:-}"` 透传未配置的变量时就是空串。
fn env_count(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Err(_) => default,
        Ok(raw) if raw.trim().is_empty() => default,
        Ok(raw) => match raw.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                tracing::warn!("[relay] {name}={raw:?} is not a positive integer, using {default}");
                default
            }
        },
    }
}

/// `RELAY_CLIENT_IP_HEADER`:空 / 没设 = 不用;不是合法头名时提示并忽略。
fn env_client_ip_header() -> Option<HeaderName> {
    let raw = std::env::var("RELAY_CLIENT_IP_HEADER").ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match HeaderName::from_bytes(raw.as_bytes()) {
        Ok(name) => Some(name),
        Err(_) => {
            tracing::warn!(
                "[relay] RELAY_CLIENT_IP_HEADER={raw:?} is not a valid header name, ignored"
            );
            None
        }
    }
}

#[tokio::main]
async fn main() {
    init_tracing();
    let bind = std::env::var("RELAY_BIND").unwrap_or_else(|_| "0.0.0.0".into());
    let port = std::env::var("RELAY_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(8080);
    let pwa_dir = std::env::var("RELAY_PWA_DIR").unwrap_or_else(|_| "./pwa".into());
    let defaults = RelayLimits::default();
    let limits = RelayLimits {
        max_connections: env_count("RELAY_MAX_CONNECTIONS", defaults.max_connections),
        max_connections_per_ip: env_count(
            "RELAY_MAX_CONNECTIONS_PER_IP",
            defaults.max_connections_per_ip,
        ),
        max_desktops: env_count("RELAY_MAX_DESKTOPS", defaults.max_desktops),
        client_ip_header: env_client_ip_header(),
        ..defaults
    };
    let state = RelayState::new()
        .with_desktop_key(std::env::var("MT_RELAY_DESKTOP_KEY").ok())
        .with_limits(limits);

    let listener = tokio::net::TcpListener::bind((bind.as_str(), port))
        .await
        .unwrap_or_else(|e| panic!("failed to bind {bind}:{port}: {e}"));
    // 打实际绑定端口(RELAY_PORT=0 时为系统分配的临时端口,测试据此定位)
    let actual_port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    tracing::info!(
        "[relay] listening on {bind}:{actual_port} (protocol v{}, pwa dir: {pwa_dir})",
        mt_relay_protocol::PROTOCOL_VERSION
    );
    if state.desktop_key_configured() {
        tracing::info!(
            "[relay] desktop key configured: desktop connections require MT_RELAY_DESKTOP_KEY"
        );
    } else {
        tracing::warn!(
            "[relay] MT_RELAY_DESKTOP_KEY is NOT set — ALL desktop connections will be rejected. \
             Set it on the relay and enter the same value in mini-term's Mobile panel."
        );
    }
    let limits = state.limits();
    tracing::info!(
        "[relay] limits: {} connections total, {} per client ({}), {} desktops, outbound queue {} frames",
        limits.max_connections,
        limits.max_connections_per_ip,
        limits.client_ip_header.as_ref().map_or_else(
            || "keyed by peer address".to_string(),
            |h| format!("keyed by {h}")
        ),
        limits.max_desktops,
        limits.outbound_queue
    );

    // 带上对端地址:按 IP 计数没配 RELAY_CLIENT_IP_HEADER 时就用它
    axum::serve(
        listener,
        app_with_pwa(state, &pwa_dir).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .expect("relay server crashed");
}
