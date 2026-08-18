/**
 * pane（终端 tab）拖拽状态管理。
 *
 * 与 fileDragState / projectDragState 同理——用 mousedown/mousemove/mouseup
 * 替代 HTML5 DnD：Tauri v2 开启 dragDropEnabled（文件拖入终端功能依赖）后，
 * 原生拖放会拦截 WebView 内部的 dragstart/dragover/drop，HTML5 拖拽整个失效。
 */

export interface PaneDragPayload {
  projectId: string;
  paneId: string;
}

/**
 * 拖拽被 Esc 取消时派发。悬停高亮由 drop 目标自己按 mousemove/mouseleave 维护，
 * 取消时鼠标往往一动不动，收不到鼠标事件——高亮靠这个事件撤下来。
 */
export const PANE_DRAG_CANCEL_EVENT = 'pane-drag-cancel';

let _payload: PaneDragPayload | null = null;
let _dragging = false;

export function isPaneDragging(): boolean {
  return _dragging;
}

export function getPaneDragPayload(): PaneDragPayload | null {
  return _dragging ? _payload : null;
}

/**
 * 在 pane tab 的 mousedown 中调用。
 * 鼠标移动超过 5px 后激活拖拽（源 tab 变半透明、body 挂 pane-dragging 让终端
 * 子元素 pointer-events 穿透）；按 Esc 中途取消；松手后抑制紧随的 click。
 */
export function initPaneDrag(
  payload: PaneDragPayload,
  el: HTMLElement,
  startX: number,
  startY: number,
): void {
  _payload = payload;
  _dragging = false;

  // 曾经激活过拖拽（含被 Esc 取消的），决定松手后要不要抑制 click
  let everDragged = false;
  let cancelled = false;

  const clearDrag = () => {
    _payload = null;
    _dragging = false;
    el.style.opacity = '';
    document.body.classList.remove('pane-dragging');
  };

  const onMove = (e: MouseEvent) => {
    if (cancelled || _dragging) return;
    if (Math.abs(e.clientX - startX) + Math.abs(e.clientY - startY) > 5) {
      _dragging = true;
      everDragged = true;
      el.style.opacity = '0.4';
      document.body.classList.add('pane-dragging');
    }
  };

  // Esc 中途取消。挂 window 的 capture：抢在 xterm 挂在 textarea 上的 keydown
  // 之前，否则这次 Esc 会被当成 \x1b 写进 PTY（与 fileDragState 同一坑）。
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key !== 'Escape' || cancelled) return;
    cancelled = true;
    const wasDragging = _dragging;
    clearDrag();
    if (!wasDragging) return;
    // 只在真的拖起来时吞掉这次 Esc；还没越过阈值就是普通按键，照常放行
    e.preventDefault();
    e.stopPropagation();
    window.dispatchEvent(new CustomEvent(PANE_DRAG_CANCEL_EVENT));
  };

  const onUp = () => {
    document.removeEventListener('mousemove', onMove);
    document.removeEventListener('mouseup', onUp);
    window.removeEventListener('keydown', onKeyDown, true);

    if (everDragged) {
      // 抑制紧随 mouseup 的 click，防止落子时误触 tab 的 onClick（激活切换）
      window.addEventListener(
        'click',
        (ce) => {
          ce.stopPropagation();
          ce.preventDefault();
        },
        { capture: true, once: true },
      );
    }

    clearDrag();
  };

  document.addEventListener('mousemove', onMove);
  document.addEventListener('mouseup', onUp);
  window.addEventListener('keydown', onKeyDown, true);
}
