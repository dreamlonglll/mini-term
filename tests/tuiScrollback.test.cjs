const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const { join } = require('node:path');
const test = require('node:test');

global.self = global;
const { Terminal } = require('@xterm/xterm');

const terminalCacheSource = readFileSync(
  join(__dirname, '..', 'src', 'utils', 'terminalCache.ts'),
  'utf8',
);

const ED3_OVERRIDE =
  "term.parser.registerCsiHandler({ final: 'J' }, (params) => params[0] === 3);";

function installCurrentEd3Policy(term) {
  if (terminalCacheSource.includes(ED3_OVERRIDE)) {
    term.parser.registerCsiHandler(
      { final: 'J' },
      (params) => params[0] === 3,
    );
  }
}

function write(term, data) {
  return new Promise((resolve) => term.write(data, resolve));
}

function bufferText(term) {
  const lines = [];
  for (let index = 0; index < term.buffer.normal.length; index += 1) {
    lines.push(term.buffer.normal.getLine(index)?.translateToString(true) ?? '');
  }
  return lines;
}

test('Codex ED2+ED3 hard-reset 删除 saved lines 后只重放 canonical transcript', async () => {
  const term = new Terminal({ cols: 24, rows: 3, scrollback: 100000 });
  installCurrentEd3Policy(term);

  await write(term, 'expanded-1\r\nexpanded-2\r\nexpanded-3\r\nexpanded-4');
  assert.ok(term.buffer.normal.baseY > 0, 'fixture 必须先形成 saved lines');

  // Codex 0.144.6 custom_terminal.rs 的 hard-reset/replay 同形序列：
  // reset margins + SGR reset + home + ED2 + ED3 + home，再重放 canonical cell。
  await write(term, '\x1b[r\x1b[0m\x1b[H\x1b[2J\x1b[3J\x1b[Hfolded transcript');

  const after = bufferText(term);
  assert.equal(
    term.buffer.normal.baseY,
    0,
    `ED3 应删除 saved lines，实际 buffer=${JSON.stringify(after)}`,
  );
  assert.equal(after.some((line) => line.includes('expanded-')), false);
  assert.equal(after.some((line) => line.includes('folded transcript')), true);
  term.dispose();
});

/** 曾经的 alt screen 拦截,仅用于在测试里复现摁回主缓冲区后的行为 */
function installLegacyAltScreenBlock(term) {
  const isAltScreenMode = (p) => {
    const v = typeof p === 'number' ? p : p[0];
    return v === 47 || v === 1047 || v === 1049;
  };
  term.parser.registerCsiHandler({ final: 'h', prefix: '?' }, (params) =>
    params.some(isAltScreenMode));
  term.parser.registerCsiHandler({ final: 'l', prefix: '?' }, (params) =>
    params.some(isAltScreenMode));
}

/** Ink/log-update 风格的一帧:相对上移回帧首 + 清到屏幕末尾 + 重画 */
function inkFrame(lines, prevLineCount) {
  const rewind = prevLineCount > 0 ? `\x1b[${prevLineCount}A` : '';
  return `${rewind}\x1b[J${lines.join('\r\n')}\r\n`;
}

/** 画 frameCount 帧「比窗口高」的界面,返回 normal buffer 里的帧头份数 */
async function drawTallFrames(term, { frameCount, frameHeight }) {
  await write(term, '\x1b[?1049h');
  let prev = 0;
  for (let i = 1; i <= frameCount; i += 1) {
    const lines = [`HEADER frame-${i}`];
    for (let r = 1; r < frameHeight; r += 1) lines.push(`  row${r} of frame-${i}`);
    await write(term, inkFrame(lines, prev));
    prev = frameHeight;
  }
  return bufferText(term).filter((l) => l.startsWith('HEADER')).length;
}

// 帧比窗口高是关键前提:帧高 <= rows 时相对上移够得到帧首,擦除是干净的,不会重复。
const TALL_FRAME = { frameCount: 4, frameHeight: 6 };
const SMALL_ROWS = 4;

test('摁回主缓冲区时,比窗口高的 Ink 帧会在 scrollback 累积残留(bug 复现)', async () => {
  const term = new Terminal({ cols: 40, rows: SMALL_ROWS, scrollback: 100000 });
  installLegacyAltScreenBlock(term);

  const headers = await drawTallFrames(term, TALL_FRAME);

  assert.equal(term.buffer.active.type, 'normal', '拦截后应仍停在主缓冲区');
  assert.equal(
    headers,
    TALL_FRAME.frameCount,
    `每帧都该留下一份擦不掉的帧头,实际 ${headers} 份`,
  );
  term.dispose();
});

test('放行 alternate-screen 后,TUI 重绘不再往 scrollback 落任何内容', async () => {
  const term = new Terminal({ cols: 40, rows: SMALL_ROWS, scrollback: 100000 });
  // 不装拦截 = 当前 terminalCache.ts 的行为

  const headers = await drawTallFrames(term, TALL_FRAME);

  assert.equal(term.buffer.active.type, 'alternate', 'TUI 应进入备用缓冲区');
  assert.equal(headers, 0, `主缓冲区不该留下任何帧头,实际 ${headers} 份`);
  assert.equal(
    bufferText(term).filter(Boolean).length,
    0,
    '主缓冲区应完全干净',
  );
  term.dispose();
});

test('CSI 策略与 100000 行容量:alt screen 必须放行,ED 全交给 xterm', () => {
  assert.doesNotMatch(
    terminalCacheSource,
    /registerCsiHandler\(\s*\{\s*final:\s*['"]J['"]\s*\}/,
    'ED0/1/2/3 必须全部交给 xterm 默认 CSI J handler',
  );
  assert.match(terminalCacheSource, /scrollback:\s*100000/);
  // 不要再把 alt screen 拦回主缓冲区 —— 会重新引入上面复现的 scrollback 残留。
  // 若将来为了保住 AI 的 scrollback 想再拦,先让上面那条复现测试过得去。
  assert.doesNotMatch(
    terminalCacheSource,
    /registerCsiHandler\(\s*\{\s*final:\s*['"][hl]['"],\s*prefix:\s*['"]\?['"]\s*\}/,
    'DECSET/DECRST 不得再被拦截,否则 TUI 重绘会在 scrollback 累积重复帧',
  );
  assert.match(
    terminalCacheSource,
    /data !== FOCUS_IN_SEQ && data !== FOCUS_OUT_SEQ[\s\S]*?term\.scrollToBottom\(\)/,
  );
  assert.match(
    terminalCacheSource,
    /term\.onResize\(\(\{ cols, rows \}\) => \{\s*invoke\('resize_pty'/,
  );
});
