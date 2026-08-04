import { invoke } from '@tauri-apps/api/core';

/**
 * 启动链路埋点（前端半场）。
 *
 * 各节点用 `markStartup` 记 epoch 毫秒（Date.now()，与 Rust 侧 T0 的 epoch 可直接对齐），
 * 窗口 show() 之后 `flushStartupTrace` 一次性 invoke 上报，由 Rust 换算到
 * 「相对进程启动 T0」的统一时间轴打印 —— 测量本身不占启动关键路径。
 */

declare global {
  interface Window {
    /** early-theme.js（index.html 内联首个脚本）执行时刻，HTML 解析最早期的锚点 */
    __earlyThemeTs?: number;
  }
}

const marks: [string, number][] = [];

export function markStartup(label: string) {
  marks.push([label, Date.now()]);
}

let flushed = false;

/** show() 之后调用；StrictMode 下 effect 双跑，只上报一次。 */
export function flushStartupTrace() {
  if (flushed) return;
  flushed = true;

  if (window.__earlyThemeTs) {
    marks.push(['early-theme.js exec', window.__earlyThemeTs]);
  }
  // timeOrigin = WebView 开始导航的时刻：它与 Rust setup 各节点的间隔
  // 反映「窗口/WebView 进程创建 + 页面开始加载」的耗时
  marks.push(['navigationStart (timeOrigin)', performance.timeOrigin]);
  const nav = performance.getEntriesByType('navigation')[0] as
    | PerformanceNavigationTiming
    | undefined;
  if (nav) {
    marks.push(['html responseEnd', performance.timeOrigin + nav.responseEnd]);
    marks.push(['domInteractive', performance.timeOrigin + nav.domInteractive]);
  }
  invoke('startup_report', { marks }).catch(() => {});
}
