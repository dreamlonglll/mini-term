import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { useAppStore, STATUS_PRIORITY } from '../store';
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
import { paneDisplayLabel } from '../utils/remoteProject';
import { useT } from '../i18n';
import type { PaneState, ProjectConfig, SplitNode } from '../types';

/**
 * 项目行悬停的 pane 预览浮层(设计: docs/plans/2026-08-08-project-pane-preview-design.md)。
 *
 * 排版为「微缩布局拼图」:按项目 SplitNode 树等比嵌套排布,整体固定尺寸永不超屏,
 * 与切过去看到的终端区所见即所得。每个分屏叶子显示 active tab 的画面,隐藏 tab
 * 以「+N」徽章示数并附其中最高优先级的状态点(隐藏 tab 挂着的 AI 黄绿灯不漏报)。
 *
 * 开闸权在 ProjectList(hasAiPane):只有跑着 AI 会话的项目才弹这张卡 —— 预览要
 * 回答的是「AI 在别的项目里跑到哪一步了」,普通 shell 项目不该被它打断。
 *
 * 纯展示、pointer-events-none:不参与命中,移出项目行即由 ProjectList 关闭。
 * 有缓存终端的 pane 读 buffer 画迷你 canvas(隐藏 tab/后台项目的 buffer 也一直
 * 在被全局 pty-output 监听更新,见 terminalCache.ts);没起过 PTY 的 pane 只有
 * 布局元数据,显示「未启动」占位。浮层打开期间 500ms 重画,预览是活的。
 */

const CARD_WIDTH = 520;
/** 拼图区固定高:约合终端区 3:2 观感,配 top 钳制任何布局都不超屏 */
const BOARD_HEIGHT = 340;

/** 与 ITheme 的 16 色字段一一对应,顺序即 ANSI 索引 */
const THEME_PALETTE_KEYS = [
  'black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white',
  'brightBlack', 'brightRed', 'brightGreen', 'brightYellow',
  'brightBlue', 'brightMagenta', 'brightCyan', 'brightWhite',
] as const;

export function themeColors(theme: Record<string, string | undefined> | undefined) {
  const t = theme ?? {};
  return {
    palette16: THEME_PALETTE_KEYS.map((k, i) => t[k] ?? DEFAULT_PREVIEW_PALETTE[i]),
    foreground: t.foreground ?? '#d8d4cc',
    background: t.background ?? '#0a0908',
  };
}

interface MiniCtx {
  tick: number;
  exitedPtyIds: Set<number>;
  labelOf: (pane: PaneState) => string;
}

function MiniPane({ pane, siblings, ctx }: {
  pane: PaneState;
  /** 同叶子的其余 tab(隐藏 tab) */
  siblings: PaneState[];
  ctx: MiniCtx;
}) {
  const t = useT();
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const autoResume = useAppStore((s) => s.config.aiAutoResume ?? true);
  const cached = pane.ptyId !== undefined ? getCachedTerminal(pane.ptyId) : undefined;
  const exited = pane.ptyId !== undefined && ctx.exitedPtyIds.has(pane.ptyId);

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
  }, [cached, ctx.tick]);

  // 隐藏 tab 中最要紧的状态:AI 在跑/刚完成藏在非激活 tab 里时,徽章旁补一个状态点。
  // 口径限于 PaneStatus —— 真正的「等确认」是 pane.attention,不在 PaneStatus 编码
  // 内,这里同样看不到(与 pane tab 栏现有行为一致)。
  const hiddenTop = siblings.length > 0
    ? siblings.reduce((best, p) => (STATUS_PRIORITY[p.status] > STATUS_PRIORITY[best.status] ? p : best))
    : null;

  return (
    <div
      className="relative w-full h-full overflow-hidden rounded-[3px] border border-[var(--border-subtle)]"
      style={{ background: 'var(--bg-terminal)' }}
    >
      {cached ? (
        // cover + 左下锚定:格子与终端宽高比不合时裁右裁顶,保住左下角的最新输出/TUI 输入区
        <canvas ref={canvasRef} className="absolute inset-0 w-full h-full object-cover object-left-bottom" />
      ) : (
        <div className="absolute inset-0 flex items-center justify-center text-xs text-[var(--text-muted)]">
          {t('projectList.preview.notStarted')}
        </div>
      )}
      {/* 标签条压在格子顶部 */}
      <div
        className="absolute top-0 inset-x-0 flex items-center gap-1 px-1.5 py-0.5 text-[10px] text-[var(--text-secondary)]"
        style={{
          background: 'color-mix(in srgb, var(--bg-overlay) 80%, transparent)',
          backdropFilter: 'blur(6px)',
        }}
      >
        <StatusDot status={pane.status} />
        {paneShowsAiSession(pane, autoResume) && (
          <BrandIcon vendor={inferVendor({ agent: pane.aiSession?.agent ?? pane.detectedAgent })} size={11} />
        )}
        <span className="truncate">{ctx.labelOf(pane)}</span>
        {siblings.length > 0 && (
          <span className="ml-auto flex items-center gap-1 flex-shrink-0">
            {hiddenTop && hiddenTop.status !== 'idle' && <StatusDot status={hiddenTop.status} />}
            <span className="text-[var(--text-muted)]">+{siblings.length}</span>
          </span>
        )}
      </div>
      {exited && (
        <div className="absolute inset-0 flex items-center justify-center bg-black/45 text-xs text-[var(--text-secondary)]">
          {t('projectList.preview.disconnected')}
        </div>
      )}
    </div>
  );
}

/** 按 SplitNode 树递归拼微缩布局:split 用 flex-grow 复现 sizes 比例,leaf 出一格 */
function MiniNode({ node, ctx }: { node: SplitNode; ctx: MiniCtx }) {
  if (node.type === 'split') {
    return (
      <div className={`flex w-full h-full gap-[2px] ${node.direction === 'vertical' ? 'flex-col' : ''}`}>
        {node.children.map((child, i) => (
          <div key={i} className="min-w-0 min-h-0" style={{ flexGrow: node.sizes?.[i] ?? 1, flexBasis: 0 }}>
            <MiniNode node={child} ctx={ctx} />
          </div>
        ))}
      </div>
    );
  }
  const active = node.panes.find((p) => p.id === node.activePaneId) ?? node.panes[0];
  if (!active) return null;
  return (
    <MiniPane
      pane={active}
      siblings={node.panes.filter((p) => p.id !== active.id)}
      ctx={ctx}
    />
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

  // 底部溢出时上移
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const h = el.getBoundingClientRect().height;
    setTop(Math.max(8, Math.min(anchorRect.top, window.innerHeight - h - 8)));
  }, [anchorRect.top, layout]);

  // layout 为 null（项目从没打开过终端）在开闸后已不可达（没 layout 就没 AI
  // pane），下面的整卡占位是防御性分支：真走到这里也得有卡头把绝对路径显出来，
  // 直接 return null 会让该项目既无浮层、也无行 title（那份 title 由 ProjectList
  // 按「没有 AI 会话」挂，与本卡互斥）。
  const ctx: MiniCtx = {
    tick,
    exitedPtyIds,
    labelOf: (pane) => paneDisplayLabel(pane, project),
  };

  return (
    <div
      ref={ref}
      // overlay-menu 带 menuPopIn 入场（语义也更贴「贴着触发点出现」），且它在
      // styles.css 的 prefers-reduced-motion 豁免名单里。内联 animation 挡不住
      // 那段的 `animation-duration: 0.01ms !important` 通配规则 —— 系统开了
      // 「减少动态效果」的机器上这个淡入原本是看不到的。
      className="overlay-menu fixed z-50 pointer-events-none rounded-md border"
      style={{
        left: Math.min(anchorRect.right + 8, window.innerWidth - CARD_WIDTH - 8),
        top,
        width: CARD_WIDTH,
        // 与 .ctx-menu 同配方:半透明皮肤(背景图主题)下毛玻璃托底,内容不透底
        background: 'var(--bg-overlay)',
        borderColor: 'var(--border-strong)',
        boxShadow: 'var(--shadow-overlay)',
        backdropFilter: 'blur(12px)',
      }}
    >
      {/* 卡头:项目名 + 绝对路径。路径原先挂在行 title 上,原生 tooltip 会盖住浮层,挪到这里 */}
      <div className="flex items-baseline gap-2 px-2 pt-2 min-w-0">
        <span className="text-xs font-medium text-[var(--text-primary)] flex-shrink-0">{project.name}</span>
        <span className="text-[11px] text-[var(--text-muted)] truncate">{project.path}</span>
      </div>
      <div className="p-2" style={{ height: BOARD_HEIGHT }}>
        {layout ? (
          <MiniNode node={layout} ctx={ctx} />
        ) : (
          <div
            className="w-full h-full flex items-center justify-center rounded-[3px] border border-dashed border-[var(--border-subtle)] text-xs text-[var(--text-muted)]"
            style={{ background: 'var(--bg-terminal)' }}
          >
            {t('projectList.preview.neverOpened')}
          </div>
        )}
      </div>
    </div>
  );
}
