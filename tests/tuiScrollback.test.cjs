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

test('alternate-screen 拦截和 100000 行容量仍保留', () => {
  assert.doesNotMatch(
    terminalCacheSource,
    /registerCsiHandler\(\s*\{\s*final:\s*['"]J['"]\s*\}/,
    'ED0/1/2/3 必须全部交给 xterm 默认 CSI J handler',
  );
  assert.match(terminalCacheSource, /scrollback:\s*100000/);
  assert.match(terminalCacheSource, /v === 47 \|\| v === 1047 \|\| v === 1049/);
  assert.match(
    terminalCacheSource,
    /registerCsiHandler\(\{ final: 'h', prefix: '\?' \}/,
  );
  assert.match(
    terminalCacheSource,
    /registerCsiHandler\(\{ final: 'l', prefix: '\?' \}/,
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
