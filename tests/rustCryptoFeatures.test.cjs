const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const path = require('node:path');
const test = require('node:test');

const projectRoot = path.resolve(__dirname, '..');

test('macOS ARM Cargo graph vendors OpenSSL', () => {
  const tree = execFileSync(
    'cargo',
    [
      'tree',
      '--manifest-path',
      'src-tauri/Cargo.toml',
      '--target',
      'aarch64-apple-darwin',
      '-e',
      'features',
      '-i',
      'openssl-sys',
    ],
    { cwd: projectRoot, encoding: 'utf8' },
  );

  assert.match(tree, /openssl-sys feature "vendored"/);
});
