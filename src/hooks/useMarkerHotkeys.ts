import { useEffect, useRef } from 'react';
import { useAppStore } from '../store';
import { scrollToMarker } from '../utils/terminalCache';
import { HOTKEYS, matchHotkey } from '../utils/hotkeys';
import { focusedPtyIdFromDom, findPaneByPtyId, resolveActivePane } from '../utils/layoutOps';

const PREV = HOTKEYS.find((h) => h.id === 'markerPrev')!;
const NEXT = HOTKEYS.find((h) => h.id === 'markerNext')!;

/**
 * AI 任务标记跳转（Ctrl+Shift+↑/↓）。
 *
 * 单独于 useGlobalHotkeys 之外，因为它要维护「这个 pane 上次跳到哪条 marker」
 * 的游标——连按方向键要沿列表连续走，而不是每次都从头/尾重来。
 */
export function useMarkerHotkeys() {
  const lastJumpRef = useRef<Map<number, string>>(new Map());

  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      const dir = matchHotkey(e, PREV) ? -1 : matchHotkey(e, NEXT) ? +1 : 0;
      if (dir === 0) return;

      const state = useAppStore.getState();
      const activeProjectId = state.activeProjectId;
      if (!activeProjectId) return;
      const layout = state.projectStates.get(activeProjectId)?.layout;
      if (!layout) return;

      // 优先用 DOM focus 定位真正聚焦的 pane（多分屏关键）；
      // 焦点不在任何 pane 内时回退到布局里第一个 leaf 的 activePaneId
      const domPtyId = focusedPtyIdFromDom();
      const ptyId = domPtyId != null && findPaneByPtyId(layout, domPtyId)
        ? domPtyId
        : resolveActivePane(layout)?.ptyId ?? null;
      if (ptyId == null) return;

      const markers = state.getMarkersForPty(ptyId);
      if (markers.length === 0) return;

      const lastId = lastJumpRef.current.get(ptyId);
      const lastIdx = lastId ? markers.findIndex((m) => m.id === lastId) : markers.length;

      let nextIdx: number;
      if (lastId && lastIdx >= 0) {
        nextIdx = lastIdx + dir;
      } else {
        nextIdx = dir === -1 ? markers.length - 1 : 0;
      }
      if (nextIdx < 0 || nextIdx >= markers.length) return;

      e.preventDefault();
      e.stopPropagation();
      const target = markers[nextIdx];
      lastJumpRef.current.set(ptyId, target.id);
      scrollToMarker(ptyId, target.xtermMarkerId);
    };

    window.addEventListener('keydown', handler, { capture: true });
    return () => window.removeEventListener('keydown', handler, { capture: true });
  }, []);
}
