/**
 * 会话列表下拉刷新的手势判定(纯函数,零运行时依赖,`tests/mobilePureLogic.test.cjs` 直接引用)。
 *
 * 判顶看的是**真正的滚动容器**(`.app-main.list-mode`,由 App 以 ref 交给 SessionList),
 * 不是手势挂载点的父元素——列表自身不滚动,父元素的 scrollTop 恒为 0,中段往回拉会误触刷新。
 */

/** 松手即刷新的下拉距离(阻尼后) */
export const PULL_TRIGGER = 64;
/** 指示器最多拉到这么高 */
export const PULL_MAX = PULL_TRIGGER + 20;
/** 位移超过这么多像素才判定手势方向(太早判,手指一抖就被当成横滑) */
export const AXIS_LOCK_SLOP = 8;

export type PullAxis = 'pending' | 'pull' | 'abandon';

/** 滚动容器是否在顶部。iOS 回弹时 scrollTop 会短暂为负;留 1px 容差吃掉亚像素。 */
export function isAtScrollTop(scrollTop: number): boolean {
  return scrollTop < 1;
}

/**
 * 按手指相对起点的位移判定这次手势是不是下拉刷新:
 * - 位移还没超过 `AXIS_LOCK_SLOP` → pending(继续观察);
 * - 横向占优(|dx| > dy,含斜向上 / 正上)→ abandon:横滑、侧滑返回、往上滚列表都不该拉出刷新;
 * - 否则(向下且纵向占优)→ pull。
 * 方向判定一次定终身,调用方锁住结果,之后不再改判。
 */
export function classifyPullGesture(dx: number, dy: number): PullAxis {
  if (Math.max(Math.abs(dx), Math.abs(dy)) < AXIS_LOCK_SLOP) return 'pending';
  if (dy <= 0 || Math.abs(dx) > dy) return 'abandon';
  return 'pull';
}

/** 阻尼:越拉越沉,避免手指一滑就整屏位移。 */
export function pullDistance(dy: number): number {
  if (dy <= 0) return 0;
  return Math.min(dy * 0.5, PULL_MAX);
}
