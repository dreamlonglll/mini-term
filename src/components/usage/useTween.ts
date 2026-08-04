import { useEffect, useRef, useState } from 'react';

const DURATION_MS = 400;
/** easeOutCubic：前快后缓，数值收敛感自然 */
const ease = (t: number) => 1 - Math.pow(1 - t, 3);

/** 单数值补间（KPI 数字滚动）；图表几何补间已由 recharts 自带动效接管 */
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
