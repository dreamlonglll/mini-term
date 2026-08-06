/**
 * 自定义文件拖拽状态管理。
 *
 * 替代 HTML5 DnD API——Tauri v2 在 Windows/WebView2 上启用 dragDropEnabled
 * 后，原生 OLE 拖拽会拦截内部 HTML5 dragover/drop 事件，导致 FileTree → Terminal
 * 的拖拽功能失效。改用 mousedown/mousemove/mouseup 实现不受此限制。
 */

/**
 * 拖拽被 Esc 取消时派发。悬停高亮由 drop 目标自己按 mousemove/mouseleave 维护，
 * 而取消时鼠标往往一动不动，收不到任何鼠标事件——高亮得靠这个事件撤下来。
 */
export const FILE_DRAG_CANCEL_EVENT = 'file-drag-cancel';

let _path: string | null = null;
let _dragging = false;

export function isFileDragging(): boolean {
  return _dragging;
}

export function getFileDragPath(): string | null {
  return _dragging ? _path : null;
}

/**
 * 在 FileTree 项的 mousedown 中调用。
 * 记录路径和起始坐标，附加全局 mousemove/mouseup/keydown 监听。
 * 鼠标移动超过 5px 后激活拖拽模式；按 Esc 中途取消。
 */
export function initFileDrag(path: string, startX: number, startY: number): void {
  _path = path;
  _dragging = false;

  // 曾经激活过拖拽（含被 Esc 取消的），决定松手后要不要抑制 click
  let everDragged = false;
  let cancelled = false;

  const clearDrag = () => {
    _path = null;
    _dragging = false;
    document.body.classList.remove('file-dragging');
  };

  const onMove = (e: MouseEvent) => {
    if (cancelled || _dragging) return;
    if (Math.abs(e.clientX - startX) + Math.abs(e.clientY - startY) > 5) {
      _dragging = true;
      everDragged = true;
      document.body.classList.add('file-dragging');
    }
  };

  // Esc 中途取消：清掉拖拽态、让 drop 目标撤掉高亮，松手时 getFileDragPath()
  // 拿到 null 自然不会写路径。mouseup 监听保留不动——click 抑制与收尾仍走同一条路径。
  // 挂 window 的 capture：capture 是 window → document → …，抢在 xterm 挂在
  // textarea 上的 keydown 之前，否则这次 Esc 会被当成 \x1b 写进 PTY。
  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key !== 'Escape' || cancelled) return;
    cancelled = true;
    const wasDragging = _dragging;
    clearDrag();
    if (!wasDragging) return;
    // 只在真的拖起来时吞掉这次 Esc；还没越过 5px 阈值就是普通按键，
    // 照常交给终端 / 弹窗，别把别人的 Esc 顺手吃了
    e.preventDefault();
    e.stopPropagation();
    window.dispatchEvent(new CustomEvent(FILE_DRAG_CANCEL_EVENT));
  };

  const onUp = () => {
    document.removeEventListener('mousemove', onMove);
    document.removeEventListener('mouseup', onUp);
    window.removeEventListener('keydown', onKeyDown, true);

    if (everDragged) {
      // 抑制紧随 mouseup 的 click 事件，防止触发 FileTree 的 onClick (打开/切换)。
      // Esc 取消的也照样抑制——用户要的是「什么都不发生」，不是退化成一次点击
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
