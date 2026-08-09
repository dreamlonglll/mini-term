/**
 * 项目 pane 预览缩略图的 buffer 提取层（纯逻辑，node --test 直测）。
 *
 * 与 terminalSnapshot.ts 同一设计：按 xterm 公开 buffer API 的形状定义结构性
 * 接口，不 import xterm 类型 —— 真 Terminal 天然兼容，测试可注入。
 *
 * 提取的是 viewport（buffer.active.viewportY 起 rows 行）：未上滚时 xterm 的
 * viewport 在 buffer 层自动跟随最新输出（对隐藏终端同样成立），上滚后则尊重
 * 用户停留的位置；TUI 的 alternate buffer 经 buffer.active 自然生效。
 *
 * 输出按「同色连续字符合并的 run」组织：canvas 侧一个 run 一次 fillText，
 * 比逐 cell 绘制少一个数量级的调用。宽字符（width=2）单独成 run —— run 内
 * 定位靠等宽字体自身 advance，CJK 字形不保证精确 2 倍半角宽，混排会漂移。
 */

import type { PreviewGrid, PreviewPaletteOptions, PreviewRun } from '../types';

export interface PreviewCellLike {
  getChars(): string;
  getWidth(): number;
  getFgColor(): number;
  isFgDefault(): boolean;
  isFgPalette(): boolean;
  isFgRGB(): boolean;
  isBold(): number;
}

export interface PreviewLineLike {
  getCell(x: number, cell?: PreviewCellLike): PreviewCellLike | undefined;
}

export interface PreviewBufferLike {
  viewportY: number;
  getLine(y: number): PreviewLineLike | undefined;
}

export interface PreviewTerminalLike {
  cols: number;
  rows: number;
  buffer: { active: PreviewBufferLike };
}

/** xterm 默认 16 色，主题缺项时的回落 */
export const DEFAULT_PREVIEW_PALETTE: string[] = [
  '#000000', '#cd0000', '#00cd00', '#cdcd00', '#0000ee', '#cd00cd', '#00cdcd', '#e5e5e5',
  '#7f7f7f', '#ff0000', '#00ff00', '#ffff00', '#5c5cff', '#ff00ff', '#00ffff', '#ffffff',
];

const CUBE_LEVELS = [0, 95, 135, 175, 215, 255];

function rgbHex(r: number, g: number, b: number): string {
  return `#${((r << 16) | (g << 8) | b).toString(16).padStart(6, '0')}`;
}

/** xterm 256 色索引 → CSS 颜色：0-15 主题色，16-231 6×6×6 立方，232-255 灰阶 */
export function resolvePaletteColor(index: number, palette16: string[]): string {
  if (index < 16) return palette16[index] ?? DEFAULT_PREVIEW_PALETTE[index] ?? '#808080';
  if (index < 232) {
    const i = index - 16;
    return rgbHex(
      CUBE_LEVELS[Math.floor(i / 36)],
      CUBE_LEVELS[Math.floor(i / 6) % 6],
      CUBE_LEVELS[i % 6],
    );
  }
  const v = 8 + (index - 232) * 10;
  return rgbHex(v, v, v);
}

function resolveCellFg(cell: PreviewCellLike, opts: PreviewPaletteOptions): string {
  if (cell.isFgRGB()) return `#${cell.getFgColor().toString(16).padStart(6, '0')}`;
  if (cell.isFgPalette()) {
    let idx = cell.getFgColor();
    // 终端惯例(等价 xterm drawBoldTextInBrightColors 默认开):粗体标准色亮化
    if (cell.isBold() && idx < 8) idx += 8;
    return resolvePaletteColor(idx, opts.palette16);
  }
  return opts.foreground;
}

export function extractPreviewGrid(
  term: PreviewTerminalLike,
  opts: PreviewPaletteOptions,
): PreviewGrid {
  const buffer = term.buffer.active;
  const top = buffer.viewportY;
  const lines: PreviewRun[][] = [];
  // getCell 的复用实例:首次调用取得后循环内复用,避免每 cell 一次对象分配
  let cellObj: PreviewCellLike | undefined;

  for (let y = 0; y < term.rows; y += 1) {
    const runs: PreviewRun[] = [];
    const line = buffer.getLine(top + y);
    if (line) {
      let run: PreviewRun | null = null;
      for (let x = 0; x < term.cols; x += 1) {
        const cell = line.getCell(x, cellObj);
        if (!cell) break;
        cellObj = cell;
        const width = cell.getWidth();
        // 宽字符的 0 宽尾 cell:前一格(width=2)已置 run=null,直接跳过
        if (width === 0) continue;
        const chars = cell.getChars();
        if (!chars || chars === ' ') {
          run = null;
          continue;
        }
        const color = resolveCellFg(cell, opts);
        if (width === 1 && run && run.color === color) {
          run.text += chars;
        } else {
          run = { col: x, text: chars, color };
          runs.push(run);
          // 宽字符单独成 run,后继字符按真实列号另起
          if (width !== 1) run = null;
        }
      }
    }
    lines.push(runs);
  }
  return { cols: term.cols, rows: term.rows, lines };
}
