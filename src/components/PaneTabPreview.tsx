import { useEffect, useRef, useState } from 'react';
import { useAppStore } from '../store';
import { getCachedTerminal, resolveTerminalFontFamily } from '../utils/terminalCache';
import { extractPreviewGrid, type PreviewTerminalLike } from '../utils/panePreview';
import { drawPreviewGrid } from '../utils/panePreviewCanvas';
import { themeColors } from './ProjectPanePreview';
import { useT } from '../i18n';
import type { PaneState } from '../types';

/**
 * 非激活 pane tab 悬停的缩略图浮层(项目行预览 ProjectPanePreview 的单格版)。
 *
 * 隐藏 tab 的内容只有切过去才看得见,这张卡回答「那个 tab 里现在是什么画面」——
 * 因此不做 AI 开闸:无论跑不跑 AI,隐藏 tab 都同样不可见。画面来源与项目预览
 * 一致:缓存终端读 buffer 画迷你 canvas(隐藏 tab 的 buffer 一直被全局
 * pty-output 监听更新,见 terminalCache.ts),打开期间 500ms 重画,预览是活的。
 *
 * 纯展示、pointer-events-none:不参与命中,移出 tab 即由 PaneGroup 关闭。
 */

const CARD_WIDTH = 380;
const CARD_HEIGHT = 232;

interface Props {
  pane: PaneState;
  /** tab 的锚点:默认贴 tab 下缘,底部放不下时翻到 tab 上方 */
  anchorRect: { left: number; top: number; bottom: number };
}

export function PaneTabPreview({ pane, anchorRect }: Props) {
  const t = useT();
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const exitedPtyIds = useAppStore((s) => s.exitedPtyIds);
  const [tick, setTick] = useState(0);
  const cached = pane.ptyId !== undefined ? getCachedTerminal(pane.ptyId) : undefined;
  const exited = pane.ptyId !== undefined && exitedPtyIds.has(pane.ptyId);

  useEffect(() => {
    const id = setInterval(() => setTick((n) => n + 1), 500);
    return () => clearInterval(id);
  }, []);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas || !cached) return;
    const { term } = cached;
    const { palette16, foreground, background } = themeColors(
      term.options.theme as Record<string, string | undefined> | undefined,
    );
    const grid = extractPreviewGrid(term as unknown as PreviewTerminalLike, { palette16, foreground });
    drawPreviewGrid(canvas, grid, {
      background,
      fontFamily: resolveTerminalFontFamily(useAppStore.getState().config.terminalFontFamily),
    });
  }, [cached, tick]);

  const left = Math.max(8, Math.min(anchorRect.left, window.innerWidth - CARD_WIDTH - 8));
  // 底部分屏的 tab 栏下方可能放不下,翻到 tab 上方
  const below = anchorRect.bottom + 6;
  const top = below + CARD_HEIGHT > window.innerHeight - 8
    ? Math.max(8, anchorRect.top - CARD_HEIGHT - 6)
    : below;

  return (
    <div
      // overlay-menu:menuPopIn 入场 + prefers-reduced-motion 豁免,同 ProjectPanePreview
      className="overlay-menu fixed z-50 pointer-events-none rounded-md border overflow-hidden"
      style={{
        left,
        top,
        width: CARD_WIDTH,
        height: CARD_HEIGHT,
        // 与 ProjectPanePreview 同配方:半透明皮肤(背景图主题)下毛玻璃托底,内容不透底
        background: 'var(--bg-overlay)',
        borderColor: 'var(--border-strong)',
        boxShadow: 'var(--shadow-overlay)',
        backdropFilter: 'blur(12px)',
      }}
    >
      {cached ? (
        // cover + 左下锚定:裁右裁顶,保住左下角的最新输出/TUI 输入区(同 MiniPane)
        <canvas ref={canvasRef} className="absolute inset-0 w-full h-full object-cover object-left-bottom" />
      ) : (
        <div className="absolute inset-0 flex items-center justify-center text-xs text-[var(--text-muted)]">
          {t('projectList.preview.notStarted')}
        </div>
      )}
      {exited && (
        <div className="absolute inset-0 flex items-center justify-center bg-black/45 text-xs text-[var(--text-secondary)]">
          {t('projectList.preview.disconnected')}
        </div>
      )}
    </div>
  );
}
