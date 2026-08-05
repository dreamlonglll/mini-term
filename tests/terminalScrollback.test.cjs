const test = require('node:test');
const assert = require('node:assert');

const {
  DEFAULT_SCROLLBACK,
  MAX_SCROLLBACK,
  resolveScrollback,
} = require('../.tmp-tests/utils/terminalScrollback.js');

// 这组用例钉住的是「renderer 不再被 scrollback 撑爆」这条约束本身。
// 曾经硬编码 100000:xterm 每行 cols × 12 字节,120 列约 1.5KB/行 → 单个终端
// 最高 150MB,而终端切项目/切 tab 都不销毁,几个 pane 就能把 WebView2 的
// renderer 推到 OOM(实测表现为整个应用"刷一下"重载,后端 PTY 全成孤儿)。

test('默认值为 10000 —— 兼顾够翻和内存', () => {
  assert.equal(DEFAULT_SCROLLBACK, 10000);
  // 是 xterm / VS Code 默认值(1000)的十倍,但只有旧硬编码值的十分之一
  assert.ok(DEFAULT_SCROLLBACK >= 1000, '不该退化到 xterm 默认的 1000');
  assert.ok(DEFAULT_SCROLLBACK <= 20000, '默认值再高就失去了这次修复的意义');
});

test('未配置时回落默认值', () => {
  assert.equal(resolveScrollback(undefined), DEFAULT_SCROLLBACK);
});

test('非法值一律回落默认值,不把 NaN 灌进 xterm', () => {
  // xterm 拿到 NaN 会把 buffer 长度算成 NaN,整个终端不可用;
  // 配置文件被手改坏时必须挡在这里。
  assert.equal(resolveScrollback(NaN), DEFAULT_SCROLLBACK);
  assert.equal(resolveScrollback(Infinity), DEFAULT_SCROLLBACK);
  assert.equal(resolveScrollback(-1), DEFAULT_SCROLLBACK);
  assert.equal(resolveScrollback(/** @type {any} */ ('8000')), DEFAULT_SCROLLBACK);
  assert.equal(resolveScrollback(/** @type {any} */ (null)), DEFAULT_SCROLLBACK);
});

test('0 是合法值(等于关掉回滚),不能被当成缺省', () => {
  assert.equal(resolveScrollback(0), 0);
});

test('合法值原样透传,小数取整', () => {
  assert.equal(resolveScrollback(5000), 5000);
  assert.equal(resolveScrollback(1), 1);
  assert.equal(resolveScrollback(3000.6), 3001);
});

test('超上限截断 —— 用户手填 1000 万不该换来一次崩溃', () => {
  assert.equal(resolveScrollback(MAX_SCROLLBACK + 1), MAX_SCROLLBACK);
  assert.equal(resolveScrollback(10_000_000), MAX_SCROLLBACK);
  assert.equal(resolveScrollback(MAX_SCROLLBACK), MAX_SCROLLBACK);
});

test('上限本身要留在一个说得过去的量级', () => {
  // 200000 行 @120 列 ≈ 300MB/终端:已经是"你自己负责"的档位,
  // 但仍在单个 renderer 扛得住的范围内。
  assert.equal(MAX_SCROLLBACK, 200000);
});
