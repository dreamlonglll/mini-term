const assert = require('node:assert/strict');
const test = require('node:test');

const { buildResumeCommand } = require('../.tmp-tests/utils/aiResume.js');

// --- 正常形态 ---

test('claude UUID 形态拼出 --resume 命令', () => {
  assert.equal(
    buildResumeCommand('claude-code', '0196c3a2-7f2e-7d31-b9c8-1a2b3c4d5e6f'),
    'claude --resume 0196c3a2-7f2e-7d31-b9c8-1a2b3c4d5e6f',
  );
});

test('codex 走 codex resume,其余 agent 值一律按 claude', () => {
  assert.equal(buildResumeCommand('codex', 'abc_DEF-123'), 'codex resume abc_DEF-123');
  assert.equal(buildResumeCommand('claude', 'abc'), 'claude --resume abc');
  assert.equal(buildResumeCommand(undefined, 'abc'), 'claude --resume abc');
});

// --- 注入面:id 来自持久化布局与会话 JSONL,均不可信 ---

test('shell 元字符一律拒绝', () => {
  for (const id of [
    'a; rm -rf ~',
    'a && curl evil',
    'a | tee',
    'a`whoami`',
    'a$(id)',
    'a b',
    'a"b',
    "a'b",
    'a\rb',
    'a\nb',
    '../../../etc/passwd',
  ]) {
    assert.equal(buildResumeCommand('codex', id), null, `应拒绝: ${JSON.stringify(id)}`);
  }
});

test('空串与超长 id 拒绝', () => {
  assert.equal(buildResumeCommand('claude', ''), null);
  assert.equal(buildResumeCommand('claude', 'x'.repeat(129)), null);
  assert.notEqual(buildResumeCommand('claude', 'x'.repeat(128)), null);
});
