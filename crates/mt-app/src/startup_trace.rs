//! 启动链路埋点:以进程启动为 T0 的统一时间轴。
//!
//! 逐条对照 `src-tauri/src/startup_trace.rs`(装机版):`init()` 在最前锁定 T0,
//! 各节点 `mark()` 往 stderr 打一行相对偏移。**没有网络、没有落盘、没有开关**
//! —— 它是给开发者看启动耗时的,不是遥测(整仓 grep `telemetry|analytics|
//! umami|plausible|posthog|sentry` 零命中,两侧都没有上报端点)。
//!
//! # 与装机版的唯一结构差异:少了「前端半场」
//!
//! Tauri 版是双进程(Rust + WebView),所以有一套 epoch 对齐的机制:前端各节点记
//! `Date.now()`,窗口 show() 之后经 `startup_report` 这个 command 一次性回传,
//! Rust 用 T0 的 epoch 时刻换算到同一轴上打印(`src/utils/startupTrace.ts`)。
//!
//! GPUI 是**单进程**:所有节点都在同一根单调钟上,`Instant::elapsed()` 直接就是
//! 相对 T0 的偏移。于是 `startup_report` command、epoch 双记、排序合并那一整套
//! 全部没有存在理由,只留 `init` + `mark`。前端那几个节点名(`main chunk exec
//! done` / `load_config resolved` / `config applied (layout restored)` …)对应的
//! 时刻在这边由 [`crate::main`] 里的同名 `mark` 调用点覆盖。

use std::sync::OnceLock;
use std::time::Instant;

static T0: OnceLock<Instant> = OnceLock::new();

/// 在 `main()` 最前调用一次,锁定 T0。
///
/// 与装机版一样自带第一条 mark —— 那条的偏移必然是 0,存在意义是让日志里有一个
/// 明确的「时间轴从这里开始」的锚,而不是从半路某个节点起头。
pub fn init() {
    let _ = T0.set(Instant::now());
    mark("run() enter");
}

/// 打一个节点。[`init`] 之前调用是**静默无操作**(与装机版 `mark` 同:
/// 它也是 `if let Some(..) = T0.get()`)。
pub fn mark(label: &str) {
    if let Some(t0) = T0.get() {
        eprintln!("{}", format_mark(t0.elapsed().as_millis(), label));
    }
}

/// 一行日志的成文。抽出来只为可测 —— 格式串照抄装机版
/// (`eprintln!("[startup +{:>5}ms] rust: {}", ..)`),两边日志能直接对着看。
fn format_mark(elapsed_ms: u128, label: &str) -> String {
    format!("[startup +{elapsed_ms:>5}ms] rust: {label}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 格式与装机版逐字符一致:5 位右对齐的毫秒数 + `ms] rust: ` + 标签。
    #[test]
    fn 日志行格式与装机版一致() {
        assert_eq!(format_mark(0, "run() enter"), "[startup +    0ms] rust: run() enter");
        assert_eq!(format_mark(12, "setup exit"), "[startup +   12ms] rust: setup exit");
        // 超过 5 位不截断,只是不再对齐(装机版 `{:>5}` 同款行为)
        assert_eq!(format_mark(123456, "x"), "[startup +123456ms] rust: x");
    }

    /// `init` 之前 `mark` 不得 panic —— 它散在启动路径上,漏掉 init
    /// (或测试里根本没 init)时必须是静默无操作,不能把进程带崩。
    #[test]
    fn 未初始化时打点是空操作() {
        // 本测试进程里 T0 大概率没被 set(单测不跑 main),即便别的测试先 init 过
        // 也只是变成正常打印 —— 两种情况都不许 panic。
        mark("测试:未初始化打点");
    }
}
