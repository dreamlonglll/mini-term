const assert = require('node:assert/strict');
const test = require('node:test');

// themePackManager 顶层 import 了 tauri api（ESM，Node 下 require 不了）。
// 被测的是两个纯函数，桩掉即可。
const Module = require('node:module');
const origLoad = Module._load;
Module._load = function (request, ...rest) {
  if (request.startsWith('@tauri-apps/')) {
    return { convertFileSrc: (p) => p, invoke: async () => {}, listen: async () => () => {} };
  }
  return origLoad.call(this, request, ...rest);
};

global.Blob = global.Blob ?? class { constructor(parts) { this.size = Buffer.byteLength(parts.join('')); } };
// 色值校验走 CSS.supports；Node 下没有 CSSOM，用例只喂 #rrggbb
global.CSS = global.CSS ?? { supports: (_prop, value) => /^#[0-9a-f]{3,8}$/i.test(String(value)) };

const { sanitizeThemeCss, parseThemePack } = require('../.tmp-tests/utils/themePackManager.js');

// ─── theme.css 卫生检查（PR #43 评审：CSS 转义可绕过原正则）───

test('CSS 转义写法的外链 url / @import 一律拒绝', () => {
  // 评审给出的三个 payload，原正则（打在未转义原文上）全部放行
  const payloads = [
    'body { background: url(\\68 ttps://evil.example/x.png); }',
    '@\\69 mport url("\\68 ttps://evil.example/a.css");',
    'body { background: url("\\68 ttps://evil.example/x.png"); }',
  ];
  for (const css of payloads) {
    assert.throws(() => sanitizeThemeCss(css), /不允许|只允许/, `应拒绝: ${css}`);
  }
});

test('未转义的外链、协议相对 url、file: 也拒绝', () => {
  for (const css of [
    'a { background: url(https://evil.example/x.png); }',
    "a { background: url('http://evil.example/x.png'); }",
    'a { background: url(//evil.example/x.png); }',
    'a { background: url(file:///etc/passwd); }',
    '@import "https://evil.example/a.css";',
  ]) {
    assert.throws(() => sanitizeThemeCss(css), /不允许|只允许/, `应拒绝: ${css}`);
  }
});

test('包内相对路径与 data: 正常放行', () => {
  for (const css of [
    'a { background: url(background.jpg); }',
    'a { background: url("./bg.png"); }',
    "a { background: url( 'sub-bg.webp' ); }",
    'a { background: url(data:image/png;base64,iVBORw0KGgo=); }',
    'a { color: red; }',
  ]) {
    assert.doesNotThrow(() => sanitizeThemeCss(css), `应放行: ${css}`);
  }
});

test('ds→mt 前缀转译保留，且返回的是原文而非转义还原后的取样', () => {
  const out = sanitizeThemeCss('[data-ds-part="x"] { --ds-theme-accent: #fff; content: "\\68 i"; }');
  assert.match(out, /data-mt-part/);
  assert.match(out, /--mt-theme-accent/);
  // 转义只用于检查取样，注入的 CSS 必须逐字保留（还原会改变 content 的字面量）
  assert.match(out, /\\68 i/);
});

// ─── theme.json 校验（PR #43 评审：image: "" 让两处口径分叉）───

const baseTheme = {
  id: 't',
  name: 'T',
  appearance: 'dark',
  colors: {
    background: '#000000',
    panel: '#111111',
    panelAlt: '#222222',
    accent: '#00ff00',
    text: '#ffffff',
    muted: '#888888',
    line: '#333333',
  },
};

test('image 为空串时归一化掉，不再被当成"有背景图"', () => {
  const def = parseThemePack('t', JSON.stringify({ ...baseTheme, image: '' }));
  // 分叉根因：hasBackgroundImage 用 !!def.image（假），
  // isTransparentThemeActive 用 image !== undefined（真）→ 终端透明还丢 WebGL，背景层却没挂
  assert.equal(def.image, undefined);
  assert.equal('image' in def, false);
});

test('image 为合法包内文件名时原样保留，含路径分量仍拒绝', () => {
  assert.equal(parseThemePack('t', JSON.stringify({ ...baseTheme, image: 'bg.jpg' })).image, 'bg.jpg');
  for (const bad of ['../x.png', 'sub/bg.png', 'sub\\bg.png']) {
    assert.throws(
      () => parseThemePack('t', JSON.stringify({ ...baseTheme, image: bad })),
      /image 必须是包内文件名/,
    );
  }
});
