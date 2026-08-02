import { useEffect, useState } from 'react';

/**
 * 弹窗退场动画的配套 hook。
 *
 * `Modal` 自己会在 `open` 转 false 时留住 DOM 播完退场动画，但这只在**它还被渲染**
 * 的前提下成立。库里大量弹窗是「关掉即卸载」的两种写法：
 *   · 组件内部 `if (!open) return null`
 *   · 父组件 `{target && <XxxModal ... />}`，onClose 里把 target 置 null
 * 两种写法都会在关闭那一刻把 `<Modal>` 整个从树上摘掉，退场动画一帧都播不到。
 *
 * 这里给出对应的两个 hook：把短路条件/数据源过一遍，关闭后子树多留一小会儿，
 * 动画就有地方播了。真正摘 DOM 的时机仍由 Modal 的 animationend 决定，
 * 这段驻留期用户看不到任何多余的东西。
 */

/** 退场驻留时长；须大于 CSS 的 `--motion-overlay-out`，与 Modal 的兜底定时器同量级 */
export const OVERLAY_EXIT_MS = 400;

/** 供 `if (!open) return null` 改写：关闭后仍返回 true，直到动画播完。 */
export function useOverlayPresence(open: boolean): boolean {
  const [lingering, setLingering] = useState(false);

  useEffect(() => {
    if (open) {
      setLingering(true);
      return;
    }
    const timer = window.setTimeout(() => setLingering(false), OVERLAY_EXIT_MS);
    return () => window.clearTimeout(timer);
  }, [open]);

  // open 期间直接为 true，不必等 effect 跑完，开弹窗不会慢一帧
  return open || lingering;
}

/**
 * 供 `{target && <XxxModal/>}` 改写：返回 `[渲染用的值, 是否打开]`。
 *
 * 调用方照旧把 state 置 null，返回值会保留最后一次的非空值供退场期间渲染
 * （弹窗里那点内容不会在淡出过程中忽然变空）。
 */
export function useOverlayValue<T>(value: T | null | undefined): [T | null, boolean] {
  const [held, setHeld] = useState<T | null>(value ?? null);

  useEffect(() => {
    if (value != null) {
      setHeld(value);
      return;
    }
    const timer = window.setTimeout(() => setHeld(null), OVERLAY_EXIT_MS);
    return () => window.clearTimeout(timer);
  }, [value]);

  return [value ?? held, value != null];
}

/**
 * 懒加载（React.lazy）弹窗的挂载门控：首次 open 前不挂载，对应 chunk 也就不会拉；
 * 首次打开后**永久**保持挂载 —— 关闭时组件仍在树上，弹窗内部的 `useOverlayPresence`
 * 才有机会播完退场动画（写成 `{open && <Lazy/>}` 会在关闭瞬间连组件带动画一起摘掉）。
 */
export function useEverOpened(open: boolean): boolean {
  const [ever, setEver] = useState(open);
  // render 期间同步置位（React 的 adjust-state-during-render 模式），开弹窗不慢一帧
  if (open && !ever) setEver(true);
  return ever;
}
