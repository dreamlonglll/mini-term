/**
 * 终端回滚行数(scrollback)的取值规则。
 *
 * 单独成文件是为了能被 Node 单测直接加载 —— terminalCache.ts 拉着 xterm、
 * Tauri API 和 store,在测试环境里跑不起来(与 terminalSnapshot / ptyWriteQueue
 * 同一处理)。
 */

/** 回滚行数默认值。见 {@link resolveScrollback} 的取值理由。 */
export const DEFAULT_SCROLLBACK = 10000;

/** 配置里能填的上限。再高就是拿 renderer 崩溃换历史,不给这个选项。 */
export const MAX_SCROLLBACK = 200000;

/**
 * 解析回滚行数配置。非法值(缺省 / NaN / 负数)一律回落默认值,超上限截断。
 *
 * 这是 WebView renderer 内存的**大头**:xterm 每行按 `Uint32Array(cols * 3)` 分配,
 * 即 cols × 12 字节;120 列约 1.5KB/行,200 列约 2.4KB/行。而终端只在 pane 真正
 * 关闭时才 dispose(切项目、切 tab 都不销毁),所以占用是
 * 「所有项目 × 所有 tab × 所有 pane」的累加,且填满后不会自行回落。
 *
 * 原先硬编码 10 万行 = 单终端最高 150-250MB,多开几个就足以把 renderer 推到 OOM
 * (renderer 被杀 → 页面重载 → 后端 PTY 全成孤儿,见 Rust 侧 kill_all_ptys)。
 * 默认 1 万行 ≈ 15MB/终端,是 xterm / VS Code 默认值(1000)的十倍,日常翻阅足够。
 */
export function resolveScrollback(value: number | undefined): number {
  if (typeof value !== 'number' || !Number.isFinite(value) || value < 0) {
    return DEFAULT_SCROLLBACK;
  }
  return Math.min(Math.round(value), MAX_SCROLLBACK);
}
