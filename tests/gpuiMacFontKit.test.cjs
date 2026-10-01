const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const path = require('node:path');
const test = require('node:test');

const projectRoot = path.resolve(__dirname, '..');

// gpui_platform 的 default feature 是空的:不开 `font-kit`,gpui_macos 退到
// NoopTextSystem,macOS 上整个界面没有一个字(issue #82)。只解析依赖图,不需要装该 target。
test('macOS ARM Cargo graph enables gpui_macos font-kit text backend', () => {
  const tree = execFileSync(
    'cargo',
    [
      'tree',
      '--manifest-path',
      'Cargo.toml',
      '--target',
      'aarch64-apple-darwin',
      '-e',
      'features',
      '-i',
      'zed-font-kit',
    ],
    { cwd: projectRoot, encoding: 'utf8' },
  );

  assert.match(tree, /gpui-pre-macos feature "font-kit"/);
});
