//! 模型价格表的网络那一跳:拉 `https://models.dev/api.json`。
//!
//! 归一(全序择优、全 0 占位价丢弃)、24h 磁盘缓存与「手工表 → 新鲜缓存 → 拉网 →
//! 过期缓存」降级链路都在 [`mt_usage::models_dev`](原先整份住在这个文件,2026-09
//! 挪过去,单测随行)。这里只剩 HTTP 客户端那一段:它用的 zed-reqwest 是 gpui 依赖树
//! 里的现成件(见 Cargo.toml 的说明),不值得为它给 mt-usage 拉一个 HTTP 客户端。

use std::path::Path;

use mt_usage::models_dev::{PricingMap, PricingSource, normalize_pricing_table};

const PRICING_URL: &str = "https://models.dev/api.json";

/// 一次 HTTPS GET + 归一。**阻塞**:DNS + TLS 握手动辄几百 ms,绝不能落主线程。
///
/// 无自定义头(旧版就是裸 `fetch(PRICING_URL)`),15s 超时是 GPUI 侧新加的
/// ——浏览器有自己的超时,`reqwest::blocking` 默认无限等。
fn fetch_models_dev() -> Result<PricingMap, String> {
    let client = crate::http::try_client().map_err(|e| e.to_string())?;
    let resp = client
        .get(PRICING_URL)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    // 自己解析而不是 `resp.json()`:那个方法要 reqwest 的 `json` feature,
    // 而它会把 serde_json 拉进 reqwest 自己的 feature 面 —— 没必要
    let text = resp.text().map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(normalize_pricing_table(&json))
}

/// 取一份可用的价格表。**整体阻塞**,调用方丢 `cx.background_executor()`。
///
/// 拉网成功时顺带写缓存;手工表 / 新鲜缓存命中即瞬时返回,不碰网络
/// (降级链路见 [`mt_usage::models_dev::ensure_pricing`])。
pub fn ensure_pricing(dir: &Path, now_ms: i64) -> Result<(PricingMap, PricingSource), String> {
    mt_usage::models_dev::ensure_pricing(dir, now_ms, fetch_models_dev)
}
