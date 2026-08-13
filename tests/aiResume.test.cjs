const assert = require('node:assert/strict');
const test = require('node:test');

const {
  buildResumeCommand,
  resolveAutoResumeCommand,
  paneShowsAiSession,
} = require('../.tmp-tests/utils/aiResume.js');

// --- 正常形态 ---

test('claude UUID 形态拼出 --resume 命令', () => {
  assert.equal(
    buildResumeCommand('claude-code', '0196c3a2-7f2e-7d31-b9c8-1a2b3c4d5e6f'),
    'claude --resume 0196c3a2-7f2e-7d31-b9c8-1a2b3c4d5e6f',
  );
});

test('codex/grok 各走各的续接命令,其余 agent 值一律按 claude', () => {
  assert.equal(buildResumeCommand('codex', 'abc_DEF-123'), 'codex resume abc_DEF-123');
  assert.equal(
    buildResumeCommand('grok', '0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e'),
    'grok --resume 0198c2f4-7e4a-7b3c-9d2e-1f0a2b3c4d5e',
  );
  assert.equal(buildResumeCommand('claude', 'abc'), 'claude --resume abc');
  assert.equal(buildResumeCommand(undefined, 'abc'), 'claude --resume abc');
});

test('grok 的 id 同样过白名单:元字符不得被拼进命令行', () => {
  assert.equal(buildResumeCommand('grok', 'a; rm -rf ~'), null);
  assert.equal(buildResumeCommand('grok', ''), null);
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

// --- 启动自动续接的否决条件 ---

const RESUMABLE = {
  enabled: true,
  resumePending: true,
  session: { agent: 'claude', sessionId: 'abc123' },
  remote: false,
};

test('四个条件齐备才写 resume 命令', () => {
  assert.equal(resolveAutoResumeCommand(RESUMABLE), 'claude --resume abc123');
  assert.equal(resolveAutoResumeCommand({ ...RESUMABLE, session: { agent: 'codex', sessionId: 'abc123' } }), 'codex resume abc123');
});

test('系统设置关掉自动续接后不写命令', () => {
  assert.equal(resolveAutoResumeCommand({ ...RESUMABLE, enabled: false }), null);
});

test('无待续接标记 / 无会话身份 / 远程 pane 均不写命令', () => {
  assert.equal(resolveAutoResumeCommand({ ...RESUMABLE, resumePending: undefined }), null);
  assert.equal(resolveAutoResumeCommand({ ...RESUMABLE, resumePending: false }), null);
  assert.equal(resolveAutoResumeCommand({ ...RESUMABLE, session: undefined }), null);
  assert.equal(resolveAutoResumeCommand({ ...RESUMABLE, remote: true }), null);
});

test('id 白名单仍然生效(开关开着也拦)', () => {
  assert.equal(
    resolveAutoResumeCommand({ ...RESUMABLE, session: { agent: 'codex', sessionId: 'a; rm -rf ~' } }),
    null,
  );
});

// --- AI 图标判定(pane 标签页 / 项目列表共用) ---

const SESSION = { agent: 'claude', sessionId: 'abc123' };

test('正在跑的 AI pane 恒亮图标,与续接开关无关', () => {
  for (const status of ['ai-working', 'ai-idle']) {
    assert.equal(paneShowsAiSession({ status }, false), true);
    assert.equal(paneShowsAiSession({ status, aiSession: SESSION, resumePending: true }, false), true);
  }
});

test('没有会话身份的普通 pane 不亮图标', () => {
  assert.equal(paneShowsAiSession({ status: 'idle' }, true), false);
  assert.equal(paneShowsAiSession({ status: 'error' }, true), false);
});

test('待续接的 pane:开关开着亮,关掉不亮(起来就是个普通 shell)', () => {
  const pending = { status: 'idle', aiSession: SESSION, resumePending: true };
  assert.equal(paneShowsAiSession(pending, true), true);
  assert.equal(paneShowsAiSession(pending, false), false);
});

test('已写过 resume(标记已清)的 pane 照常亮,不受开关影响', () => {
  const resumed = { status: 'idle', aiSession: SESSION, resumePending: undefined };
  assert.equal(paneShowsAiSession(resumed, true), true);
  assert.equal(paneShowsAiSession(resumed, false), true);
});
