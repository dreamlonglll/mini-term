import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useAppStore } from '../store';
import { collectLeaves } from '../utils/layoutOps';
import { getCachedTerminal, resolveTerminalFontFamily } from '../utils/terminalCache';
import {
  extractPreviewGrid,
  DEFAULT_PREVIEW_PALETTE,
  type PreviewTerminalLike,
} from '../utils/panePreview';
import { drawPreviewGrid } from '../utils/panePreviewCanvas';
import { StatusDot } from './StatusDot';
import { BrandIcon } from './BrandIcon';
import { inferVendor } from '../utils/inferVendor';
import { paneShowsAiSession } from '../utils/aiResume';
import { isRemoteProject, remotePaneLabel } from '../utils/remoteProject';
import { useT } from '../i18n';
import type { PaneState, ProjectConfig } from '../types';

/**
 * 项目行悬停的 pane 预览浮层(设计: docs/plans/2026-08-08-project-pane-preview-design.md)。
 *
 * 纯展示、pointer-events-none:不参与命中,移出项目行即由 ProjectList 关闭。
 * 有缓存终端的 pane 读 buffer 画迷你 canvas(隐藏 tab/后台项目的 buffer 也一直
 * 在被全局 pty-output 监听更新,见 terminalCache.ts);没起过 PTY 的 pane 只有
 * 布局元数据,显示「未启动」占位。浮层打开期间 500ms 重画,预览是活的。
 */

const CARD_WIDTH = 520;
const MAX_PANES = 4;
/** 与 ITheme 的 16 色字段一一对应,顺序即 ANSI 索引 */
const THEME_PALETTE_KEYS = [
  'black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white',
  'brightBlack', 'brightRed', 'brightGreen', 'brightYellow',
  'brightBlue', 'brightMagenta', 'brightCyan', 'brightWhite',
] as const;

function themeColors(theme: Record<string, string | undefined> | undefined) {
  const t = theme ?? {};
  return {
    palette16: THEME_PALETTE_KEYS.map((k, i) => t[k] ?? DEFAULT_PREVIEW_PALETTE[i]),
    foreground: t.foreground ?? '#d8d4cc',
    background: t.background ?? '#0a0908',
  };
}

function PaneThumb({ pane, label, tick, exited }: {
  pane: PaneState;
  label: string;
  tick: number;
  exited: boolean;
}) {
  const t = useT();
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const autoResume = useAppStore((s) => s.config.aiAutoResume ?? true);
  const cached = pane.ptyId !== undefined ? getCachedTerminal(pane.ptyId) : undefined;

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

  return (
    <div className="px-2 pt-2 last:pb-2">
      <div className="flex items-center gap-1.5 pb-1 text-xs text-[var(--text-secondary)]">
        <StatusDot status={pane.status} />
        {paneShowsAiSession(pane, autoResume) && (
          <BrandIcon vendor={inferVendor({ agent: pane.aiSession?.agent ?? pane.detectedAgent })} size={12} />
        )}
        <span className="truncate">{label}</span>
      </div>
      {cached ? (
        <div className="relative rounded-[var(--radius-sm)] overflow-hidden border border-[var(--border-subtle)]">
          {/* 高终端裁顶留底:底部是最新输出/TUI 输入区,正是缩略图要看的 */}
          <canvas ref={canvasRef} className="block w-full h-auto max-h-[240px] object-cover object-bottom" />
          {exited && (
            <div className="absolute inset-0 flex items-center justify-center bg-black/45 text-xs text-[var(--text-secondary)]">
              {t('projectList.preview.disconnected')}
            </div>
          )}
        </div>
      ) : (
        <div className="h-10 flex items-center justify-center rounded-[var(--radius-sm)] border border-dashed border-[var(--border-subtle)] text-xs text-[var(--text-muted)]">
          {t('projectList.preview.notStarted')}
        </div>
      )}
    </div>
  );
}

interface Props {
  project: ProjectConfig;
  /** 项目行的锚点:top 对齐行顶,left 取行右缘 + 8 */
  anchorRect: { top: number; right: number };
}

export function ProjectPanePreview({ project, anchorRect }: Props) {
  const t = useT();
  const layout = useAppStore((s) => s.projectStates.get(project.id)?.layout ?? null);
  const exitedPtyIds = useAppStore((s) => s.exitedPtyIds);
  const [tick, setTick] = useState(0);
  const ref = useRef<HTMLDivElement>(null);
  const [top, setTop] = useState(anchorRect.top);

  useEffect(() => {
    const id = setInterval(() => setTick((n) => n + 1), 500);
    return () => clearInterval(id);
  }, []);

  // 底部溢出时上移;高度随 pane 数变化,layout 变了重新钳
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const h = el.getBoundingClientRect().height;
    setTop(Math.max(8, Math.min(anchorRect.top, window.innerHeight - h - 8)));
  }, [anchorRect.top, layout]);

  if (!layout) return null;

  const remote = isRemoteProject(project);
  const remoteLabel = remote ? remotePaneLabel(project) : undefined;
  const entries: { pane: PaneState; leafIndex: number }[] = [];
  collectLeaves(layout).forEach((leaf, leafIndex) => {
    for (const pane of leaf.panes) entries.push({ pane, leafIndex });
  });
  const shown = entries.slice(0, MAX_PANES);
  const hiddenCount = entries.length - shown.length;

  return (
    <div
      ref={ref}
      className="fixed z-50 pointer-events-none rounded-md border"
      style={{
        left: Math.min(anchorRect.right + 8, window.innerWidth - CARD_WIDTH - 8),
        top,
        width: CARD_WIDTH,
        // 与 .ctx-menu 同配方:半透明皮肤(背景图主题)下毛玻璃托底,内容不透底
        background: 'var(--bg-overlay)',
        borderColor: 'var(--border-strong)',
        boxShadow: 'var(--shadow-overlay)',
        backdropFilter: 'blur(12px)',
        animation: 'overlayFadeIn 0.15s ease-out',
      }}
    >
      {/* 卡头:项目名 + 绝对路径。路径原先挂在行 title 上,原生 tooltip 会盖住浮层,挪到这里 */}
      <div className="flex items-baseline gap-2 px-2 pt-2 min-w-0">
        <span className="text-xs font-medium text-[var(--text-primary)] flex-shrink-0">{project.name}</span>
        <span className="text-[11px] text-[var(--text-muted)] truncate">{project.path}</span>
      </div>
      {shown.map(({ pane, leafIndex }, i) => (
        <div key={pane.id}>
          {/* 分屏叶子之间加分隔线,同叶子内的 tab 连排 */}
          {i > 0 && leafIndex !== shown[i - 1].leafIndex && (
            <div className="mx-2 mt-2 border-t border-[var(--border-subtle)]" />
          )}
          <PaneThumb
            pane={pane}
            label={pane.customTitle || (remote ? remoteLabel! : pane.shellName)}
            tick={tick}
            exited={pane.ptyId !== undefined && exitedPtyIds.has(pane.ptyId)}
          />
        </div>
      ))}
      {hiddenCount > 0 && (
        <div className="px-2 py-1.5 text-xs text-[var(--text-muted)]">
          {t('projectList.preview.morePanes', { count: hiddenCount })}
        </div>
      )}
    </div>
  );
}
