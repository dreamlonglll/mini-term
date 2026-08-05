const assert = require('node:assert/strict');
const test = require('node:test');

const { inferVendor } = require('../.tmp-tests/utils/inferVendor.js');

// --- agent(hook 上报)优先于 command ---

test('agent 命中时优先于 command', () => {
  assert.equal(inferVendor({ agent: 'claude', command: 'codex resume abc' }), 'claude');
  assert.equal(inferVendor({ agent: 'codex', command: 'claude --resume abc' }), 'openai');
});

test('agent 未命中时回退 command 推断', () => {
  assert.equal(inferVendor({ agent: '', command: 'gemini chat' }), 'gemini');
  assert.equal(inferVendor({ command: 'opencode' }), 'opencode');
});

// --- 各关键词 ---

test('命令文本关键词映射', () => {
  assert.equal(inferVendor({ command: 'claude --dangerously-skip-permissions' }), 'claude');
  assert.equal(inferVendor({ command: 'ANTHROPIC_BASE_URL=x claude' }), 'claude');
  assert.equal(inferVendor({ command: 'codex' }), 'openai');
  assert.equal(inferVendor({ command: 'gemini' }), 'gemini');
  assert.equal(inferVendor({ command: 'grok chat' }), 'grok');
  assert.equal(inferVendor({ command: 'qwen code' }), 'qwen');
  assert.equal(inferVendor({ command: 'deepseek-cli' }), 'deepseek');
  assert.equal(inferVendor({ command: 'copilot suggest' }), 'copilot');
  assert.equal(inferVendor({ command: 'ollama run llama3' }), 'ollama');
});

// --- pi(多模型 harness,规则排在最前) ---

test('pi 命令与 agent 都识别为 pi', () => {
  assert.equal(inferVendor({ command: 'pi' }), 'pi');
  assert.equal(inferVendor({ agent: 'pi' }), 'pi');
  assert.equal(inferVendor({ command: 'pi -c' }), 'pi');
});

test('pi 承载别家模型时仍显示 harness 自己', () => {
  assert.equal(inferVendor({ command: 'pi --model claude-sonnet-5' }), 'pi');
  assert.equal(inferVendor({ command: 'pi --model gpt-5' }), 'pi');
});

test('pi 规则不误伤含 pi 字母的命令/厂商', () => {
  assert.equal(inferVendor({ command: 'pip install requests' }), null);
  assert.equal(inferVendor({ command: 'pixi run build' }), null);
  assert.equal(inferVendor({ command: 'ping example.com' }), null);
  assert.equal(inferVendor({ command: 'copilot suggest' }), 'copilot');
  assert.equal(inferVendor({ command: 'opencode' }), 'opencode');
});

test('大小写不敏感', () => {
  assert.equal(inferVendor({ command: 'Claude' }), 'claude');
  assert.equal(inferVendor({ command: 'CODEX' }), 'openai');
});

// --- o1–o4 正则边界 ---

test('o1–o4 系列按独立词识别为 openai', () => {
  assert.equal(inferVendor({ command: 'run o1 task' }), 'openai');
  assert.equal(inferVendor({ command: 'model=o3' }), 'openai');
});

test('o1–o4 不误伤普通单词', () => {
  assert.equal(inferVendor({ command: 'foo3 build' }), null);
  assert.equal(inferVendor({ command: 'echo hello' }), null);
  assert.equal(inferVendor({ command: 'do1thing' }), null);
});

// --- 未知输入回退 ---

test('未知/空输入返回 null', () => {
  assert.equal(inferVendor({}), null);
  assert.equal(inferVendor({ command: 'zsh -l' }), null);
  assert.equal(inferVendor({ agent: 'mystery-agent' }), null);
});
