import { useEffect, useRef, useState } from 'react';

const DURATION_MS = 400;
/** easeOutCubic：前快后缓，数值收敛感自然 */
const ease = (t: number) => 1 - Math.pow(1 - t, 3);

/**
 * 数值数组补间：目标值变化时从当前显示值 rAF 平滑过渡到新值（~0.4s）。
 * 长度变化（如切 range 桶数不同）直接跳变，避免错位补间。
 * WKWebView 不支持 CSS 过渡 SVG path `d`，图表几何统一走本 hook。
 */
export function useTweenedNumbers(target: number[]): number[] {
  const [display, setDisplay] = useState(target);
  const currentRef = useRef(target);
  const key = target.join(',');
  useEffect(() => {
    const from = currentRef.current;
    const to = target;
    if (from.length !== to.length) {
      currentRef.current = to;
      setDisplay(to);
      return;
    }
    if (from.every((v, i) => v === to[i])) return;
    let raf = 0;
    const start = performance.now();
    const tick = (now: number) => {
      const t = Math.min((now - start) / DURATION_MS, 1);
      const k = ease(t);
      const next = to.map((v, i) => from[i] + (v - from[i]) * k);
      currentRef.current = next;
      setDisplay(next);
      if (t < 1) raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
    // target 数组每次渲染都是新引用，按内容键控避免空跑
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  return currentRef.current.length === target.length ? display : target;
}

/** 单数值补间（KPI 数字滚动） */
export function useTweenedNumber(target: number): number {
  const [display, setDisplay] = useState(target);
  const currentRef = useRef(target);
  useEffect(() => {
    const from = currentRef.current;
    if (from === target) return;
    let raf = 0;
    const start = performance.now();
    const tick = (now: number) => {
      const t = Math.min((now - start) / DURATION_MS, 1);
      const next = from + (target - from) * ease(t);
      currentRef.current = next;
      setDisplay(next);
      if (t < 1) raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [target]);
  return display;
}
