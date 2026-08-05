const assert = require('node:assert/strict');
const test = require('node:test');

const {
  canonicalModelKey,
  normalizePricingTable,
} = require('../.tmp-tests/utils/modelPricing.js');

// models.dev 是 provider → models 两层结构，同一个模型被几十家 provider 以不同
// id、不同价登记。归一后必须是「canonical 键 → 唯一权威价」，否则碰撞会被推迟
// 到后端才发生，而后端看不见 provider，只能听 HashMap 迭代顺序。

test('canonicalModelKey 与后端 canonical() 同规则', () => {
  assert.equal(canonicalModelKey('anthropic/claude-opus-4.7'), 'claude-opus-4-7');
  assert.equal(canonicalModelKey('Claude-Opus-4-8'), 'claude-opus-4-8');
  assert.equal(canonicalModelKey('claude-opus-5@eu'), 'claude-opus-5');
  assert.equal(canonicalModelKey('gpt-5.3-codex@pin'), 'gpt-5-3-codex');
  // 先取 `/` 后段再剥 `@`，顺序不能反
  assert.equal(canonicalModelKey('workers-ai/@cf/zai-org/glm-5.2'), 'glm-5-2');
});

/** 现网真实碰撞组：四个原始键全部塌成 claude-opus-5，requesty 欧区价高 10% */
function opus5Api() {
  return {
    anthropic: {
      models: {
        'claude-opus-5': { cost: { input: 5, output: 25, cache_read: 0.5, cache_write: 6.25 } },
      },
    },
    crossmodel: {
      models: {
        'anthropic/claude-opus-5': {
          cost: { input: 5, output: 25, cache_read: 0.5, cache_write: 6.25 },
        },
      },
    },
    'google-vertex-anthropic': {
      models: {
        'claude-opus-5@default': {
          cost: { input: 5, output: 25, cache_read: 0.5, cache_write: 6.25 },
        },
      },
    },
    requesty: {
      models: {
        'claude-opus-5@eu': {
          cost: { input: 5.5, output: 27.5, cache_read: 0.55, cache_write: 6.88 },
        },
      },
    },
  };
}

test('碰撞组只留一个 canonical 键，且取一方 provider 的价', () => {
  const table = normalizePricingTable(opus5Api());
  assert.deepEqual(Object.keys(table), ['claude-opus-5']);
  assert.deepEqual(table['claude-opus-5'], {
    input: 5e-6,
    output: 25e-6,
    cacheRead: 0.5e-6,
    cacheWrite: 6.25e-6,
  });
});

test('择优与 provider / 模型的遍历顺序无关', () => {
  const forward = opus5Api();
  const reversed = Object.fromEntries(Object.entries(forward).reverse());
  assert.deepEqual(normalizePricingTable(reversed), normalizePricingTable(forward));
});

test('全 0 占位价被丢弃，不参与择优', () => {
  // kenari 这类订阅制 provider 用全 0 表示「不单独计费」；收下会把该模型整段
  // 成本抹成 0，比查不到价更糟（查不到还有后端的三锚点均价兜底）
  const table = normalizePricingTable({
    kenari: { models: { 'gpt-5-6-sol': { cost: { input: 0, output: 0 } } } },
    openai: {
      models: { 'gpt-5.6-sol': { cost: { input: 5, output: 30, cache_read: 0.5 } } },
    },
  });
  assert.equal(table['gpt-5-6-sol'].input, 5e-6);
  assert.equal(table['gpt-5-6-sol'].output, 30e-6);
});

test('一方 provider 压过聚合商，即便聚合商先被遍历到', () => {
  // provider id 字典序上 aggregator < openai，先被遍历到；仍须让位
  const table = normalizePricingTable({
    aggregator: { models: { 'gpt-5.5': { cost: { input: 9, output: 9 } } } },
    openai: { models: { 'gpt-5.5': { cost: { input: 5, output: 30 } } } },
  });
  assert.equal(table['gpt-5-5'].input, 5e-6);
});

test('同优先级下，缓存单价齐全者胜出', () => {
  // 两家都不是一方 provider：缓存单价缺失会退化成 0，而 Claude 侧 cache_read
  // 常占成本七成以上，取错会把总额砍掉大半
  const table = normalizePricingTable({
    aaa: { models: { 'some-model': { cost: { input: 3, output: 15 } } } },
    zzz: {
      models: { 'some-model': { cost: { input: 3, output: 15, cache_read: 0.3 } } },
    },
  });
  assert.equal(table['some-model'].cacheRead, 0.3e-6);
});

test('缺 input/output 的条目被跳过，非法输入返回空表', () => {
  const table = normalizePricingTable({
    p: { models: { broken: { cost: { input: 1 } }, ok: { cost: { input: 1, output: 2 } } } },
  });
  assert.deepEqual(Object.keys(table), ['ok']);
  assert.deepEqual(normalizePricingTable(null), {});
  assert.deepEqual(normalizePricingTable('nope'), {});
  assert.deepEqual(normalizePricingTable({ p: {} }), {});
});
