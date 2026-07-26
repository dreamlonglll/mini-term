/**
 * 「当前屏幕上压着什么」的唯一真相。
 *
 * 应用里有三类覆盖物，各自实现方式不同：
 *   - React 弹窗（`components/Modal.tsx`）
 *   - 命令式弹窗（`utils/prompt.ts` 的 confirm/alert/prompt）
 *   - 右键菜单（`utils/contextMenu.ts`）
 *
 * 它们原先各管各的 Esc，且注册在不同节点（window vs document）的 capture 上。
 * capture 是 window → document → …，所以 Modal 的 handler **必然**先于命令式
 * 弹窗和菜单执行 —— 弹窗里再弹一个 confirm，按 Esc 关掉的是底下那层弹窗，
 * confirm 反而孤零零留在最上面。同样地，`useGlobalHotkeys` 靠 App 手工维护的
 * `modalOpen` 布尔判断"有没有弹窗"，漏掉了一多半覆盖物，导致查看器开着时
 * Ctrl+Shift+W 照样去关后面的终端。
 *
 * 统一到这个栈之后：谁在栈顶谁吃 Esc，`isOverlayOpen()` 是全局唯一判据。
 */

export type OverlayKind = 'modal' | 'prompt' | 'menu';

interface Entry {
  id: number;
  kind: OverlayKind;
}

const stack: Entry[] = [];
let seq = 0;

/** 压入一层覆盖物，返回它的 id（出栈时用）。 */
export function pushOverlay(kind: OverlayKind): number {
  const id = ++seq;
  stack.push({ id, kind });
  return id;
}

/** 弹出指定覆盖物（幂等；不要求是栈顶，异常关闭顺序也不会卡住栈）。 */
export function popOverlay(id: number): void {
  const idx = stack.findIndex((e) => e.id === id);
  if (idx >= 0) stack.splice(idx, 1);
}

/** 该覆盖物是否在栈顶 —— 只有栈顶才该响应 Esc。 */
export function isTopOverlay(id: number): boolean {
  return stack.length > 0 && stack[stack.length - 1].id === id;
}

/** 当前是否有任何覆盖物打开（全局快捷键据此让路）。 */
export function isOverlayOpen(): boolean {
  return stack.length > 0;
}

/** 栈顶覆盖物的类型；空栈返回 null。 */
export function topOverlayKind(): OverlayKind | null {
  return stack.length > 0 ? stack[stack.length - 1].kind : null;
}
