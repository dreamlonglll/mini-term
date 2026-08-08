const assert = require('node:assert/strict');
const test = require('node:test');

global.self = global;
const { Terminal } = require('@xterm/xterm');

const {
  extractPreviewGrid,
  resolvePaletteColor,
  DEFAULT_PREVIEW_PALETTE,
} = require('../.tmp-tests/utils/panePreview.js');

/** 测试用 16 色板：索引即辨识度（值不必是真实主题色） */
const PALETTE16 = [
  '#000000', '#aa0000', '#00aa00', '#aaaa00',
  '#0000aa', '#aa00aa', '#00aaaa', '#aaaaaa',
  '#555555', '#ff5555', '#55ff55', '#ffff55',
  '#5555ff', '#ff55ff', '#55ffff', '#ffffff',
];
const FOREGROUND = '#d8d4cc';

function write(term, data) {
  return new Promise((resolve) => term.write(data, resolve));
}

function extract(term) {
  return extractPreviewGrid(term, { palette16: PALETTE16, foreground: FOREGROUND });
}

/** 平铺一行的 runs 为 [col, text, color] 三元组，断言更紧凑 */
function flat(line) {
  return line.map((r) => [r.col, r.text, r.color]);
}

test('基本提取:纯文本落在第 0 行,颜色为默认前景', async () => {
  const term = new Terminal({ cols: 20, rows: 5 });
  await write(term, 'hello');
  const grid = extract(term);
  assert.equal(grid.cols, 20);
  assert.equal(grid.rows, 5);
  assert.deepEqual(flat(grid.lines[0]), [[0, 'hello', FOREGROUND]]);
  // 其余行全空白,不产生 run
  assert.deepEqual(grid.lines[1], []);
  term.dispose();
});

test('SGR 16 色:同色连续字符合并为一个 run,颜色切换断开', async () => {
  const term = new Terminal({ cols: 20, rows: 3 });
  await write(term, '\x1b[31mred\x1b[0m plain');
  const grid = extract(term);
  assert.deepEqual(flat(grid.lines[0]), [
    [0, 'red', PALETTE16[1]],
    [4, 'plain', FOREGROUND],
  ]);
  term.dispose();
});

test('粗体 + 标准色按终端惯例加亮为 bright 色', async () => {
  const term = new Terminal({ cols: 20, rows: 3 });
  await write(term, '\x1b[1;31mB\x1b[0m');
  const grid = extract(term);
  assert.deepEqual(flat(grid.lines[0]), [[0, 'B', PALETTE16[9]]]);
  term.dispose();
});

test('256 色与 truecolor 前景解析为 RGB', async () => {
  const term = new Terminal({ cols: 20, rows: 3 });
  await write(term, '\x1b[38;5;196mA\x1b[0m\x1b[38;2;18;52;86mB\x1b[0m');
  const grid = extract(term);
  assert.deepEqual(flat(grid.lines[0]), [
    [0, 'A', '#ff0000'],
    [1, 'B', '#123456'],
  ]);
  term.dispose();
});

test('宽字符占两列:尾 cell 跳过,后续字符列号正确', async () => {
  const term = new Terminal({ cols: 20, rows: 3 });
  await write(term, '中x');
  const grid = extract(term);
  // 宽字符单独成 run(width!=1 断开),x 从第 2 列开始
  assert.deepEqual(flat(grid.lines[0]), [
    [0, '中', FOREGROUND],
    [2, 'x', FOREGROUND],
  ]);
  term.dispose();
});

test('viewport 跟随:滚出一屏后提取的是最新可视区', async () => {
  const term = new Terminal({ cols: 20, rows: 3, scrollback: 100 });
  const lines = [];
  for (let i = 1; i <= 10; i += 1) lines.push(`line-${i}`);
  await write(term, lines.join('\r\n'));
  const grid = extract(term);
  assert.equal(grid.rows, 3);
  assert.deepEqual(flat(grid.lines[0]), [[0, 'line-8', FOREGROUND]]);
  assert.deepEqual(flat(grid.lines[2]), [[0, 'line-10', FOREGROUND]]);
  term.dispose();
});

test('用户上滚后提取跟随 viewport 而非底部', async () => {
  const term = new Terminal({ cols: 20, rows: 3, scrollback: 100 });
  const lines = [];
  for (let i = 1; i <= 10; i += 1) lines.push(`line-${i}`);
  await write(term, lines.join('\r\n'));
  term.scrollLines(-2);
  const grid = extract(term);
  assert.deepEqual(flat(grid.lines[0]), [[0, 'line-6', FOREGROUND]]);
  term.dispose();
});

test('alternate buffer(TUI 画面)提取的是 alt 屏内容', async () => {
  const term = new Terminal({ cols: 20, rows: 3 });
  await write(term, 'shell prompt');
  await write(term, '\x1b[?1049h\x1b[2J\x1b[HTUI');
  const grid = extract(term);
  assert.deepEqual(flat(grid.lines[0]), [[0, 'TUI', FOREGROUND]]);
  // 退出 alt 屏回到主 buffer(空格断 run,提取为两段)
  await write(term, '\x1b[?1049l');
  const back = extract(term);
  assert.deepEqual(flat(back.lines[0]), [
    [0, 'shell', FOREGROUND],
    [6, 'prompt', FOREGROUND],
  ]);
  term.dispose();
});

test('resolvePaletteColor:16 色立方与灰阶边界', () => {
  assert.equal(resolvePaletteColor(3, PALETTE16), PALETTE16[3]);
  assert.equal(resolvePaletteColor(16, PALETTE16), '#000000');
  assert.equal(resolvePaletteColor(196, PALETTE16), '#ff0000');
  assert.equal(resolvePaletteColor(231, PALETTE16), '#ffffff');
  assert.equal(resolvePaletteColor(232, PALETTE16), '#080808');
  assert.equal(resolvePaletteColor(255, PALETTE16), '#eeeeee');
});

test('DEFAULT_PREVIEW_PALETTE 提供 16 项回落色板', () => {
  assert.equal(DEFAULT_PREVIEW_PALETTE.length, 16);
  for (const c of DEFAULT_PREVIEW_PALETTE) assert.match(c, /^#[0-9a-f]{6}$/);
});
