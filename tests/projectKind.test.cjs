const assert = require('node:assert/strict');
const test = require('node:test');

const { classifyProject, parsePackageDeps } = require('../.tmp-tests/utils/projectKind.js');

// --- 标记文件直判 ---

test('各标记文件映射到对应类型', () => {
  assert.equal(classifyProject(new Set(['pom.xml'])), 'java');
  assert.equal(classifyProject(new Set(['build.gradle'])), 'java');
  assert.equal(classifyProject(new Set(['build.gradle.kts'])), 'java');
  assert.equal(classifyProject(new Set(['Cargo.toml'])), 'rust');
  assert.equal(classifyProject(new Set(['go.mod'])), 'go');
  assert.equal(classifyProject(new Set(['pyproject.toml'])), 'python');
  assert.equal(classifyProject(new Set(['requirements.txt'])), 'python');
  assert.equal(classifyProject(new Set(['pubspec.yaml'])), 'flutter');
  assert.equal(classifyProject(new Set(['composer.json'])), 'php');
});

// --- 多命中优先级:从具体到泛化 ---

test('多标记文件命中时按优先级取更具体者', () => {
  // Tauri 项目:Cargo.toml + package.json → rust 优先
  assert.equal(classifyProject(new Set(['Cargo.toml', 'package.json'])), 'rust');
  assert.equal(classifyProject(new Set(['pom.xml', 'package.json'])), 'java');
});

// --- package.json 依赖细分 ---

test('package.json 按依赖细分前端框架', () => {
  const files = new Set(['package.json']);
  assert.equal(classifyProject(files, { vue: '^3.0.0' }), 'vuejs');
  assert.equal(classifyProject(files, { next: '15.0.0', react: '19.0.0' }), 'nextjs');
  assert.equal(classifyProject(files, { react: '^19.1.0' }), 'react');
  assert.equal(classifyProject(files, { svelte: '^5.0.0' }), 'svelte');
  assert.equal(classifyProject(files, { vite: '^7.0.0' }), 'vite');
  assert.equal(classifyProject(files, { express: '^4.0.0' }), 'nodejs');
  assert.equal(classifyProject(files), 'nodejs');
});

// --- 空目录 / 识别不出 ---

test('空目录与未知布局返回 null', () => {
  assert.equal(classifyProject(new Set()), null);
  assert.equal(classifyProject(new Set(['README.md', '.gitignore'])), null);
});

// --- parsePackageDeps ---

test('parsePackageDeps 合并 dependencies 与 devDependencies', () => {
  const deps = parsePackageDeps(JSON.stringify({
    dependencies: { react: '^19.0.0' },
    devDependencies: { vite: '^7.0.0' },
  }));
  assert.deepEqual(deps, { react: '^19.0.0', vite: '^7.0.0' });
});

test('parsePackageDeps 解析失败返回 undefined', () => {
  assert.equal(parsePackageDeps('not json'), undefined);
  assert.equal(parsePackageDeps('null'), undefined);
});
