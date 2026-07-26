const assert = require('node:assert/strict');
const test = require('node:test');

const { isWslPath, windowsPathToWsl } = require('../.tmp-tests/utils/wslPath.js');

// --- isWslPath（既有行为，回归保护）---

test('isWslPath 识别四种 WSL UNC 形式', () => {
  assert.equal(isWslPath('\\\\wsl$\\Ubuntu\\home\\u'), true);
  assert.equal(isWslPath('\\\\wsl.localhost\\Ubuntu\\home\\u'), true);
  assert.equal(isWslPath('\\\\?\\UNC\\wsl$\\Ubuntu\\home'), true);
  assert.equal(isWslPath('\\\\?\\UNC\\wsl.localhost\\Ubuntu\\home'), true);
  // host 大小写不敏感
  assert.equal(isWslPath('\\\\WSL$\\Ubuntu\\home'), true);
});

test('isWslPath 拒绝普通 UNC 与盘符路径', () => {
  assert.equal(isWslPath('\\\\server\\share\\folder'), false);
  assert.equal(isWslPath('C:\\proj'), false);
  assert.equal(isWslPath('/home/u/proj'), false);
  assert.equal(isWslPath(''), false);
});

// --- windowsPathToWsl（issue #36 新增）---

test('windowsPathToWsl 把盘符路径转成 /mnt 形式', () => {
  assert.equal(
    windowsPathToWsl('C:\\Users\\u\\AppData\\Local\\Temp\\clip-1.png'),
    '/mnt/c/Users/u/AppData/Local/Temp/clip-1.png',
  );
  // 盘符小写化：WSL 的 automount 挂载点是小写盘符
  assert.equal(windowsPathToWsl('D:\\Git\\mini-term'), '/mnt/d/Git/mini-term');
  assert.equal(windowsPathToWsl('d:\\Git'), '/mnt/d/Git');
});

test('windowsPathToWsl 处理 verbatim 前缀与正斜杠输入', () => {
  assert.equal(windowsPathToWsl('\\\\?\\C:\\Temp\\a.png'), '/mnt/c/Temp/a.png');
  // 已经是正斜杠的盘符路径同样要能转
  assert.equal(windowsPathToWsl('C:/Temp/a.png'), '/mnt/c/Temp/a.png');
  // 混合分隔符不能有 `\` 残留
  assert.equal(windowsPathToWsl('C:/Temp\\sub\\a.png'), '/mnt/c/Temp/sub/a.png');
});

test('windowsPathToWsl 对盘符根返回挂载点', () => {
  assert.equal(windowsPathToWsl('C:\\'), '/mnt/c/');
});

test('windowsPathToWsl 对非盘符路径返回 null', () => {
  // UNC（含 WSL UNC 自身）不参与转换，调用方原样粘贴
  assert.equal(windowsPathToWsl('\\\\wsl$\\Ubuntu\\home\\u\\a.png'), null);
  assert.equal(windowsPathToWsl('\\\\server\\share\\a.png'), null);
  // 已经是 POSIX 路径
  assert.equal(windowsPathToWsl('/tmp/a.png'), null);
  assert.equal(windowsPathToWsl(''), null);
  // 只有盘符没有分隔符：拼不出合法路径
  assert.equal(windowsPathToWsl('C:'), null);
});
