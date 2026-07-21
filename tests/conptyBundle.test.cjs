const assert = require('node:assert/strict');
const { mkdtemp, mkdir, readFile, rm, writeFile } = require('node:fs/promises');
const { tmpdir } = require('node:os');
const { join } = require('node:path');
const { pathToFileURL } = require('node:url');
const test = require('node:test');

const repoRoot = join(__dirname, '..');
const windowsConfigPath = join(repoRoot, 'src-tauri', 'tauri.windows.conf.json');
const stagingModuleUrl = pathToFileURL(
  join(repoRoot, 'scripts', 'stage-conpty.mjs'),
).href;

async function loadStagingModule() {
  return import(stagingModuleUrl);
}

function minimalPe(machine) {
  const buffer = Buffer.alloc(0x100);
  buffer.writeUInt16LE(0x5a4d, 0);
  buffer.writeUInt32LE(0x80, 0x3c);
  buffer.write('PE\0\0', 0x80, 'ascii');
  buffer.writeUInt16LE(machine, 0x84);
  return buffer;
}

async function writePackageFixture(root) {
  const x64Native = join(root, 'runtimes', 'win-x64', 'native');
  const x64Host = join(root, 'build', 'native', 'runtimes', 'x64');
  const arm64Host = join(root, 'build', 'native', 'runtimes', 'arm64');
  await Promise.all([
    mkdir(x64Native, { recursive: true }),
    mkdir(x64Host, { recursive: true }),
    mkdir(arm64Host, { recursive: true }),
  ]);
  await Promise.all([
    writeFile(join(x64Native, 'conpty.dll'), minimalPe(0x8664)),
    writeFile(join(x64Host, 'OpenConsole.exe'), minimalPe(0x8664)),
    writeFile(join(arm64Host, 'OpenConsole.exe'), minimalPe(0xaa64)),
  ]);
}

test('Windows bundle 将 staging 目录映射到 portable-conpty 资源目录', async () => {
  const config = JSON.parse(await readFile(windowsConfigPath, 'utf8'));
  assert.deepEqual(config.bundle?.resources, {
    'resources/portable-conpty': 'portable-conpty',
  });
});

test('官方 ConPTY 包版本、来源与 SHA-256 固定且可诊断', async () => {
  const { CONPTY_PACKAGE } = await loadStagingModule();
  assert.equal(CONPTY_PACKAGE.releaseTag, 'v1.24.11911.0');
  assert.equal(CONPTY_PACKAGE.version, '1.24.260710001');
  assert.equal(
    CONPTY_PACKAGE.url,
    'https://github.com/microsoft/terminal/releases/download/v1.24.11911.0/Microsoft.Windows.Console.ConPTY.1.24.260710001.nupkg',
  );
  assert.equal(
    CONPTY_PACKAGE.sha256,
    '9382ad7becb7e4d84e300578d8e4f4df28f43d979d9055d978c42913c47e0e9d',
  );
});

test('x64 发布目标 staging 同包 x64 DLL 与 x64/ARM64 host', async (t) => {
  const tempRoot = await mkdtemp(join(tmpdir(), 'mini-term-conpty-'));
  t.after(() => rm(tempRoot, { recursive: true, force: true }));
  const packageRoot = join(tempRoot, 'package');
  const outputDir = join(tempRoot, 'staged');
  await writePackageFixture(packageRoot);

  const { stagePortableConptyFromDirectory, validatePortableConptyLayout } =
    await loadStagingModule();
  await stagePortableConptyFromDirectory({
    target: 'x86_64-pc-windows-msvc',
    packageRoot,
    outputDir,
  });

  const layout = await validatePortableConptyLayout(outputDir);
  assert.deepEqual(layout.files, [
    'conpty.dll',
    'x64/OpenConsole.exe',
    'arm64/OpenConsole.exe',
  ]);
  assert.deepEqual(layout.machines, [0x8664, 0x8664, 0xaa64]);
});

test('不支持的发布目标明确拒绝，不能静默复制错误架构', async (t) => {
  const tempRoot = await mkdtemp(join(tmpdir(), 'mini-term-conpty-'));
  t.after(() => rm(tempRoot, { recursive: true, force: true }));
  const packageRoot = join(tempRoot, 'package');
  await writePackageFixture(packageRoot);
  const { stagePortableConptyFromDirectory } = await loadStagingModule();

  await assert.rejects(
    stagePortableConptyFromDirectory({
      target: 'aarch64-pc-windows-msvc',
      packageRoot,
      outputDir: join(tempRoot, 'staged'),
    }),
    /仅支持 Windows x64 发布目标/,
  );
});
