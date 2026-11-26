// scripts/pack-extensions.mjs 的守门测试(CI 的 `node --test tests/*.test.cjs` 会跑)。
//
// 两件事:
// 1. 脚本自身的行为:确定性(同一输入两次打出同一个 sha256)、内容寻址的资产名、清单 /
//    离线包 / verify 的往返、各类非法输入一律报错;
// 2. **与 Rust 侧安装代码的往返**:crates/mt-extensions/tests/fixtures/ 是用本脚本打出来的
//    一份夹具(两个 helper 扩展 + 清单 + 离线包),这里断言脚本现在仍逐字节产出同样的东西;
//    那边的 `tests/fixtures.rs` 断言 mt-extensions 装得上它。两头一夹,脚本与安装代码的口径
//    谁漂了都会有一边红。改了打包格式就跑 `UPDATE_EXT_FIXTURES=1 node --test tests/packExtensions.test.cjs`
//    重写夹具,再跑 `cargo test -p mt-extensions` 确认 Rust 侧仍认。
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const { mkdtemp, rm } = require('node:fs/promises');
const fs = require('node:fs');
const { tmpdir } = require('node:os');
const { join } = require('node:path');
const { pathToFileURL } = require('node:url');
const test = require('node:test');

const repoRoot = join(__dirname, '..');
const scriptPath = join(repoRoot, 'scripts', 'pack-extensions.mjs');
const fixturesDir = join(repoRoot, 'crates', 'mt-extensions', 'tests', 'fixtures');

async function loadPacker() {
  return import(pathToFileURL(scriptPath).href);
}

async function tempDir(t) {
  const dir = await mkdtemp(join(tmpdir(), 'mini-term-pack-ext-'));
  t.after(() => rm(dir, { recursive: true, force: true }));
  return dir;
}

function run(args) {
  return execFileSync(process.execPath, [scriptPath, ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });
}

/** 夹具:两个假 helper → 两个扩展 zip + 片段 + 清单 + 离线包。返回产物路径。 */
async function buildFixture(packer, inputDir, outDir) {
  const assetsDir = join(outDir, 'assets');
  fs.mkdirSync(assetsDir, { recursive: true });
  const fragments = [];
  for (const [id, target] of [
    ['git-helper-linux-x86_64', 'x64'],
    ['git-helper-linux-aarch64', 'arm64'],
  ]) {
    const { fragment, zip, asset } = packer.packComponent(id, [
      { path: join(inputDir, target, 'mt-git-helper'), executable: true },
    ]);
    fs.writeFileSync(join(assetsDir, asset), zip);
    fragments.push(fragment);
  }
  const manifest = packer.buildManifest({
    platform: 'windows-x64',
    appVersion: '0.0.0-fixture',
    tag: 'v0.0.0-fixture',
    offlineBundle: 'Mini-Term_0.0.0-fixture_windows-x64-extensions.zip',
    fragments,
    assetsDir,
  });
  const manifestPath = join(outDir, 'extensions.json');
  fs.writeFileSync(manifestPath, packer.serializeManifest(manifest));
  const bundlePath = join(outDir, manifest.offlineBundle);
  fs.writeFileSync(bundlePath, packer.buildBundle(manifestPath, assetsDir));
  return { manifest, manifestPath, assetsDir, bundlePath };
}

test('扩展 zip 逐字节可复现,资产名内容寻址', async (t) => {
  const packer = await loadPacker();
  const dir = await tempDir(t);
  const helper = join(dir, 'mt-git-helper');
  fs.writeFileSync(helper, Buffer.from('\x7fELF fake helper\n'));
  const a = packer.packComponent('git-helper-linux-x86_64', [{ path: helper, executable: true }]);
  const b = packer.packComponent('git-helper-linux-x86_64', [{ path: helper, executable: true }]);
  assert.equal(a.fragment.sha256, b.fragment.sha256);
  assert.deepEqual(a.zip, b.zip);
  assert.equal(a.asset, `mt-ext_git-helper-linux-x86_64_${a.fragment.sha256.slice(0, 12)}.zip`);
  assert.equal(a.fragment.size, a.zip.length);
  assert.deepEqual(a.fragment.files, [
    {
      name: 'mt-git-helper',
      sha256: packer.sha256(fs.readFileSync(helper)),
      size: fs.statSync(helper).size,
      executable: true,
    },
  ]);
  // 读回来:固定 mtime 与权限位、内容一致
  const [entry] = packer.readZip(a.zip);
  assert.equal(entry.name, 'mt-git-helper');
  assert.equal(entry.mode, 0o100755);
  assert.deepEqual(entry.data, fs.readFileSync(helper));
  // 可执行标记只影响权限位
  const plain = packer.packComponent('git-helper-linux-x86_64', [{ path: helper, executable: false }]);
  assert.equal(packer.readZip(plain.zip)[0].mode, 0o100644);
  assert.notEqual(plain.fragment.sha256, a.fragment.sha256);
});

test('清单 / 离线包 / verify 往返', async (t) => {
  const dir = await tempDir(t);
  const helper = join(dir, 'in', 'mt-git-helper');
  fs.mkdirSync(join(dir, 'in'));
  fs.writeFileSync(helper, Buffer.from('helper bytes'));
  const out = join(dir, 'out');
  const fragment = JSON.parse(run(['pack', '--id', 'git-helper-macos-aarch64', '--exe', helper, '--out-dir', out]));
  assert.ok(fs.existsSync(join(out, fragment.asset)));
  assert.ok(fs.existsSync(join(out, 'git-helper-macos-aarch64.fragment.json')));

  const manifestPath = join(dir, 'extensions.json');
  run([
    'manifest', '--platform', 'macos-arm64', '--app-version', '1.15.0', '--tag', 'v1.15.0',
    '--offline-bundle', 'Mini-Term_1.15.0_macos-arm64-extensions.zip',
    '--fragments', out, '--assets', out, '--out', manifestPath,
  ]);
  const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));
  assert.equal(manifest.schema, 1);
  assert.equal(manifest.platform, 'macos-arm64');
  assert.deepEqual(manifest.components, [fragment]);

  const bundlePath = join(dir, 'bundle.zip');
  run(['bundle', '--manifest', manifestPath, '--assets', out, '--out', bundlePath]);
  const packer = await loadPacker();
  const entries = packer.readZip(fs.readFileSync(bundlePath));
  assert.deepEqual(entries.map((e) => e.name), [fragment.asset, 'extensions.json']);
  // 离线包里的扩展 zip 原样 store
  assert.deepEqual(entries[0].data, fs.readFileSync(join(out, fragment.asset)));
  run(['verify', '--manifest', manifestPath, '--assets', out]);

  // 资产被改过一个字节:manifest / bundle / verify 都拒
  const tampered = fs.readFileSync(join(out, fragment.asset));
  tampered[tampered.length - 1] ^= 0xff;
  fs.writeFileSync(join(out, fragment.asset), tampered);
  for (const args of [
    ['verify', '--manifest', manifestPath, '--assets', out],
    ['bundle', '--manifest', manifestPath, '--assets', out, '--out', bundlePath],
    ['manifest', '--platform', 'macos-arm64', '--app-version', '1', '--tag', 'v1', '--fragments', out, '--assets', out, '--out', manifestPath],
  ]) {
    assert.throws(() => run(args), undefined, args[0]);
  }
});

test('非法输入一律报错', async (t) => {
  const packer = await loadPacker();
  const dir = await tempDir(t);
  const ok = join(dir, 'mt-git-helper');
  fs.writeFileSync(ok, 'x');
  const dotted = join(dir, '.hidden');
  fs.writeFileSync(dotted, 'x');
  assert.throws(() => packer.packComponent('git-helper-linux-riscv64', [{ path: ok }]), /未知扩展 id/);
  assert.throws(() => packer.packComponent('ssh-tools', []), /至少要一个文件/);
  assert.throws(() => packer.packComponent('ssh-tools', [{ path: dotted }]), /文件名不合规/);
  assert.throws(() => packer.packComponent('ssh-tools', [{ path: ok }, { path: ok }]), /重复/);
  const { fragment } = packer.packComponent('ssh-tools', [{ path: ok }]);
  const base = { appVersion: '1.0.0', tag: 'v1.0.0', fragments: [fragment] };
  assert.throws(() => packer.buildManifest({ ...base, platform: 'windows-arm64' }), /未知平台键/);
  assert.throws(() => packer.buildManifest({ ...base, platform: 'linux-x64', tag: 'v1/../x' }), /--tag/);
  assert.throws(
    () => packer.buildManifest({ ...base, platform: 'linux-x64', fragments: [fragment, fragment] }),
    /出现了两次/,
  );
  assert.throws(
    () => packer.readFragments([join(dir, 'missing-dir-file.json')]),
    /ENOENT/,
  );
  for (const bad of ['', '.x', 'a/b', 'a\\b', 'a..b', 'CON', 'com1.exe', 'x.', 'a:b']) {
    assert.equal(packer.isSafeFileName(bad), false, JSON.stringify(bad));
  }
  for (const good of ['mt-git-helper', 'mt-ssh-cli.exe', 'libmt_syntax_pack.dylib']) {
    assert.equal(packer.isSafeFileName(good), true, good);
  }
});

test('与 Rust 侧共用的夹具仍由本脚本逐字节产出', async (t) => {
  const packer = await loadPacker();
  const inputDir = join(fixturesDir, 'input');
  if (process.env.UPDATE_EXT_FIXTURES === '1') {
    fs.rmSync(join(fixturesDir, 'assets'), { recursive: true, force: true });
    for (const name of fs.readdirSync(fixturesDir)) {
      if (name.endsWith('.zip') || name === 'extensions.json') fs.rmSync(join(fixturesDir, name));
    }
    await buildFixture(packer, inputDir, fixturesDir);
  }
  const dir = await tempDir(t);
  const fresh = await buildFixture(packer, inputDir, dir);
  // 清单按 JSON 语义比(文本文件可能被编辑器改换行);zip 逐字节比
  assert.deepEqual(
    JSON.parse(fs.readFileSync(join(fixturesDir, 'extensions.json'), 'utf8')),
    fresh.manifest,
  );
  for (const component of fresh.manifest.components) {
    assert.deepEqual(
      fs.readFileSync(join(fixturesDir, 'assets', component.asset)),
      fs.readFileSync(join(fresh.assetsDir, component.asset)),
      component.asset,
    );
  }
  // 离线包里带着清单副本(文本),所以只比条目名与扩展 zip 的字节
  const committed = packer.readZip(fs.readFileSync(join(fixturesDir, fresh.manifest.offlineBundle)));
  const rebuilt = packer.readZip(fs.readFileSync(fresh.bundlePath));
  assert.deepEqual(committed.map((e) => e.name), rebuilt.map((e) => e.name));
  for (let index = 0; index < rebuilt.length - 1; index++) {
    assert.deepEqual(committed[index].data, rebuilt[index].data, rebuilt[index].name);
  }
  packer.verifyAssets(join(fixturesDir, 'extensions.json'), join(fixturesDir, 'assets'));
});
