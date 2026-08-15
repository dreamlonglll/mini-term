/**
 * 把 xterm 的字符宽度测量量化到整设备像素，对齐 WebGL 与 DOM 渲染器的格宽。
 *
 * 上游差异（xterm v6，实测自打包产物）：
 *   WebGL:  device.char.width = Math.floor(charSizeService.width × dpr)
 *   DOM:    device.char.width = charSizeService.width × dpr        ← 不取整
 * 二者的 cell.width 都是 char.width + Math.round(letterSpacing)，唯一分歧就是
 * 那个 floor。字体步进带小数时（FiraCode 14px = 8.4px），DOM 每格比 WebGL 宽
 * 零点几像素，80 列累计 30px+ —— 带背景图主题（透明模式只能走 DOM 渲染）的
 * 字符间距因此肉眼可见地比内置主题（WebGL）松。
 *
 * letterSpacing 选项救不了：两个渲染器都对它 Math.round，负小数穿不过去。
 * 唯一的单点收口是两边共同读取的 CharSizeService——把测量结果量化为
 * floor(w × dpr) / dpr 之后，DOM 的 w×dpr 与 WebGL 的 floor(w×dpr) 恒等：
 * WebGL 侧 floor 退化为恒等变换（零行为变化），DOM 侧格宽与 WebGL 对齐，
 * DomRenderer._setDefaultSpacing 自会按量化格宽给行容器算出相应的负
 * letter-spacing（字形以自然宽度微溢出格子，与 WebGL 的 atlas 溢出同貌）。
 *
 * DPR 变化无需额外监听：RenderService.handleDevicePixelRatioChange 会重调
 * charSizeService.measure()，包装器每次都读当时的 devicePixelRatio。
 *
 * 经 any 反射私有字段（_core._charSizeService._measureStrategy），与
 * terminalCache 的 resetRenderStateForPty 同一风险口径：xterm 升级改名时
 * 静默失效、退回未补偿行为（DOM 间距变宽），不抛错不影响功能。
 */

import type { Terminal } from '@xterm/xterm';

/** 已包装标记，防止对同一 strategy 重复套壳 */
const QUANTIZED_FLAG = '__mtQuantized';

export function quantizeCharMeasurement(term: Terminal): void {
  try {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const svc = (term as any)._core?._charSizeService;
    const strategy = svc?._measureStrategy;
    if (!svc || !strategy || typeof strategy.measure !== 'function') return;
    if (strategy[QUANTIZED_FLAG]) return;
    const original = strategy.measure.bind(strategy);
    strategy.measure = () => {
      const result = original();
      const dpr = window.devicePixelRatio || 1;
      if (result && typeof result.width === 'number' && result.width > 0) {
        // max(1, …) 兜底：极小字号 × 低 dpr 下 floor 出 0 会被 _validateAndSet
        // 拒收，宽度卡在旧值
        const target = Math.max(1, Math.floor(result.width * dpr));
        let w = target / dpr;
        // FP 防线：先除后乘可能落在 target 下方一个 ulp（如 9 − 1e-15），
        // WebGL 侧的 floor 会因此再少 1 设备像素，反而改变了 WebGL 的现状。
        // 逐 ulp 上调直到乘回去不塌方（现实中至多 1~2 轮，上限只是保险）。
        for (let i = 0; i < 4 && Math.floor(w * dpr) < target; i++) {
          w *= 1 + Number.EPSILON;
        }
        result.width = w;
      }
      return result;
    };
    strategy[QUANTIZED_FLAG] = true;
    // open() 时的首测未经包装，立即以量化口径重测一次；
    // 结果有变化时 CharSizeService 自己发 onCharSizeChange 通知渲染器
    svc.measure();
  } catch (e) {
    console.warn('字符宽度量化失败（仅影响 DOM 渲染下的字符间距）:', e);
  }
}
