import type { PreviewGrid } from '../types';

/**
 * 预览网格 → canvas 位图。
 *
 * 内部分辨率按 8px 字号的真实 cell 网格建(cols×4.8 × rows×10.8),CSS 再等比
 * 缩到卡片宽度 —— 文字在内部分辨率下清晰,缩小显示的观感等同真截图缩小,
 * 避免直接按卡宽算 cell(亚像素字号 fillText 全糊)。
 */
const FONT_SIZE = 8;
const CELL_W = FONT_SIZE * 0.6;
/** 与终端 lineHeight: 1.35 同口径,行密度一致 */
const CELL_H = FONT_SIZE * 1.35;

export function drawPreviewGrid(
  canvas: HTMLCanvasElement,
  grid: PreviewGrid,
  opts: { background: string; fontFamily: string },
): void {
  const w = Math.max(1, Math.ceil(grid.cols * CELL_W));
  const h = Math.max(1, Math.ceil(grid.rows * CELL_H));
  if (canvas.width !== w) canvas.width = w;
  if (canvas.height !== h) canvas.height = h;
  const ctx = canvas.getContext('2d');
  if (!ctx) return;

  ctx.fillStyle = opts.background;
  ctx.fillRect(0, 0, w, h);
  ctx.font = `${FONT_SIZE}px ${opts.fontFamily}`;
  ctx.textBaseline = 'middle';

  for (let y = 0; y < grid.lines.length; y += 1) {
    const runs = grid.lines[y];
    if (runs.length === 0) continue;
    const cy = y * CELL_H + CELL_H / 2;
    for (let i = 0; i < runs.length; i += 1) {
      const run = runs[i];
      // maxWidth 钳到下一 run 起点(或行尾):fallback 字形 advance 与等宽口径
      // 不齐时被压缩而不是越界盖住右侧内容
      const endCol = i + 1 < runs.length ? runs[i + 1].col : grid.cols;
      ctx.fillStyle = run.color;
      ctx.fillText(run.text, run.col * CELL_W, cy, Math.max(1, (endCol - run.col) * CELL_W));
    }
  }
}
