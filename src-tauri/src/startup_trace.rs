//! 启动链路埋点：以进程启动为 T0 的统一时间轴。
//!
//! Rust 侧各节点直接 `mark()` 打相对偏移；前端各节点记 `Date.now()`（epoch ms），
//! 窗口 show() 后经 `startup_report` 一次性上报，用 T0 的 epoch 时刻换算到同一
//! 时间轴统一打印 —— 两侧日志可直接对齐，测量本身不占启动路径。

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static T0: OnceLock<(Instant, f64)> = OnceLock::new();

/// 在 run() 最前调用一次，锁定 T0（单调钟 + epoch 双记：单调钟算偏移，epoch 对齐前端）。
pub fn init() {
    let epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0);
    let _ = T0.set((Instant::now(), epoch_ms));
    mark("run() enter");
}

pub fn mark(label: &str) {
    if let Some((t0, _)) = T0.get() {
        eprintln!("[startup +{:>5}ms] rust: {}", t0.elapsed().as_millis(), label);
    }
}

/// 前端时间线上报：`marks` 为 [label, epoch_ms] 列表，换算相对 T0 后排序打印。
#[tauri::command]
pub fn startup_report(marks: Vec<(String, f64)>) {
    let t0_epoch = match T0.get() {
        Some((_, e)) => *e,
        None => return,
    };
    let mut rows: Vec<(f64, String)> = marks
        .into_iter()
        .map(|(label, ts)| (ts - t0_epoch, label))
        .collect();
    rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    eprintln!("[startup] ── 前端时间线（相对 Rust 进程启动 T0）──");
    for (dt, label) in rows {
        eprintln!("[startup +{:>5}ms] web:  {}", dt as i64, label);
    }
    if let Some((t0, _)) = T0.get() {
        eprintln!(
            "[startup +{:>5}ms] ── 时间线上报完毕 ──",
            t0.elapsed().as_millis()
        );
    }
}
