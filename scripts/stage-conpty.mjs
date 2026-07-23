// 下载、校验并整理 Microsoft 官方便携 ConPTY 运行时。
//
// mini-term 当前只发布 Windows x64 进程。按 Microsoft NuGet targets 的约定，
// x64 进程必须携带 x64 conpty.dll，以及 x64/ARM64 两种原生系统 host。
// 产物由 tauri.windows.conf.json 映射到应用 resource_dir()/portable-conpty。

import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync } from 'node:fs';
import {
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  stat,
  writeFile,
} from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const WINDOWS_X64_TARGET = 'x86_64-pc-windows-msvc';

export const CONPTY_PACKAGE = Object.freeze({
  releaseTag: 'v1.24.11911.0',
  version: '1.24.260710001',
  fileName: 'Microsoft.Windows.Console.ConPTY.1.24.260710001.nupkg',
  url: 'https://github.com/microsoft/terminal/releases/download/v1.24.11911.0/Microsoft.Windows.Console.ConPTY.1.24.260710001.nupkg',
  sha256: '9382ad7becb7e4d84e300578d8e4f4df28f43d979d9055d978c42913c47e0e9d',
  files: Object.freeze([
    Object.freeze({
      source: 'runtimes/win-x64/native/conpty.dll',
      target: 'conpty.dll',
      machine: 0x8664,
      sha256: '39fba2713e2495117b1591ae8c32a3b904bea7aa66069cf7815e2844c76d75d8',
    }),
    Object.freeze({
      source: 'build/native/runtimes/x64/OpenConsole.exe',
      target: 'x64/OpenConsole.exe',
      machine: 0x8664,
      sha256: 'b7fd936c2668b87b9ecf7b3366dc6568afc1c6f981874cba3e955a1c35cf8160',
    }),
    Object.freeze({
      source: 'build/native/runtimes/arm64/OpenConsole.exe',
      target: 'arm64/OpenConsole.exe',
      machine: 0xaa64,
      sha256: 'ed7622fd0d3bedc9ab9f122f5e58edf0def9e7999224f52dd395ba9f54edbe09',
    }),
  ]),
});

const CACHE_DIR = join('src-tauri', '.conpty-cache');
export const STAGED_CONPTY_DIR = join('src-tauri', 'resources', 'portable-conpty');

function assertSupportedTarget(target) {
  if (target !== WINDOWS_X64_TARGET) {
    throw new Error(
      `[stage-conpty] 仅支持 Windows x64 发布目标 ${WINDOWS_X64_TARGET}，收到 ${target}`,
    );
  }
}

async function sha256(filePath) {
  const hash = createHash('sha256');
  hash.update(await readFile(filePath));
  return hash.digest('hex');
}

async function verifyHash(filePath, expected, label) {
  const actual = await sha256(filePath);
  if (actual !== expected) {
    throw new Error(
      `[stage-conpty] ${label} SHA-256 不匹配：expected=${expected} actual=${actual}`,
    );
  }
}

export async function readPeMachine(filePath) {
  const info = await stat(filePath);
  if (!info.isFile() || info.size === 0) {
    throw new Error(`[stage-conpty] 资源不是非空文件：${filePath}`);
  }

  const bytes = await readFile(filePath);
  if (bytes.length < 0x40 || bytes.readUInt16LE(0) !== 0x5a4d) {
    throw new Error(`[stage-conpty] 资源不是合法 PE 文件（缺少 MZ）：${filePath}`);
  }
  const peOffset = bytes.readUInt32LE(0x3c);
  if (
    peOffset + 6 > bytes.length ||
    bytes.toString('binary', peOffset, peOffset + 4) !== 'PE\0\0'
  ) {
    throw new Error(`[stage-conpty] 资源不是合法 PE 文件（缺少 PE header）：${filePath}`);
  }
  return bytes.readUInt16LE(peOffset + 4);
}

export async function validatePortableConptyLayout(
  outputDir,
  { verifyOfficialHashes = false } = {},
) {
  const machines = [];
  for (const file of CONPTY_PACKAGE.files) {
    const filePath = join(outputDir, ...file.target.split('/'));
    const machine = await readPeMachine(filePath);
    if (machine !== file.machine) {
      throw new Error(
        `[stage-conpty] ${file.target} PE machine 不匹配：expected=0x${file.machine.toString(16)} actual=0x${machine.toString(16)}`,
      );
    }
    if (verifyOfficialHashes) {
      await verifyHash(filePath, file.sha256, file.target);
    }
    machines.push(machine);
  }
  return {
    files: CONPTY_PACKAGE.files.map((file) => file.target),
    machines,
  };
}

export async function stagePortableConptyFromDirectory({
  target,
  packageRoot,
  outputDir,
  verifyOfficialHashes = false,
}) {
  assertSupportedTarget(target);

  for (const file of CONPTY_PACKAGE.files) {
    const source = join(packageRoot, ...file.source.split('/'));
    const machine = await readPeMachine(source);
    if (machine !== file.machine) {
      throw new Error(
        `[stage-conpty] ${file.source} PE machine 不匹配：expected=0x${file.machine.toString(16)} actual=0x${machine.toString(16)}`,
      );
    }
    if (verifyOfficialHashes) {
      await verifyHash(source, file.sha256, file.source);
    }

    const destination = join(outputDir, ...file.target.split('/'));
    await mkdir(dirname(destination), { recursive: true });
    await copyFile(source, destination);
    console.log(`[stage-conpty] ${source} -> ${destination}`);
  }

  return validatePortableConptyLayout(outputDir, { verifyOfficialHashes });
}

// .nupkg 是 zip 容器；Git Bash 等环境 PATH 上的 GNU tar 不识别 zip，
// Windows 下固定使用系统自带的 bsdtar（System32\tar.exe）
function tarExecutable() {
  if (process.platform === 'win32') {
    const systemTar = join(process.env.SystemRoot ?? 'C:\\Windows', 'System32', 'tar.exe');
    if (existsSync(systemTar)) return systemTar;
  }
  return 'tar';
}

async function downloadPackage(destination) {
  console.log(`[stage-conpty] 下载 ${CONPTY_PACKAGE.url}`);
  const response = await fetch(CONPTY_PACKAGE.url, { redirect: 'follow' });
  if (!response.ok) {
    throw new Error(
      `[stage-conpty] 下载失败：HTTP ${response.status} ${response.statusText}`,
    );
  }
  await mkdir(dirname(destination), { recursive: true });
  await writeFile(destination, Buffer.from(await response.arrayBuffer()));
}

export async function stagePortableConpty({
  target,
  cacheDir = CACHE_DIR,
  outputDir = STAGED_CONPTY_DIR,
} = {}) {
  assertSupportedTarget(target);
  const packagePath = join(cacheDir, CONPTY_PACKAGE.fileName);

  let packageUsable = false;
  try {
    await verifyHash(packagePath, CONPTY_PACKAGE.sha256, CONPTY_PACKAGE.fileName);
    packageUsable = true;
    console.log(`[stage-conpty] 使用已校验缓存 ${packagePath}`);
  } catch (error) {
    console.warn(`[stage-conpty] 缓存不可用，将重新下载：${error.message}`);
  }

  if (!packageUsable) {
    await downloadPackage(packagePath);
    await verifyHash(packagePath, CONPTY_PACKAGE.sha256, CONPTY_PACKAGE.fileName);
  }

  const extractionRoot = await mkdtemp(join(tmpdir(), 'mini-term-conpty-'));
  try {
    execFileSync(tarExecutable(), ['-xf', packagePath, '-C', extractionRoot], {
      stdio: 'inherit',
    });
    await rm(outputDir, { recursive: true, force: true });
    const layout = await stagePortableConptyFromDirectory({
      target,
      packageRoot: extractionRoot,
      outputDir,
      verifyOfficialHashes: true,
    });
    console.log(
      `[stage-conpty] 完成 version=${CONPTY_PACKAGE.version} sha256=${CONPTY_PACKAGE.sha256}`,
    );
    return layout;
  } finally {
    await rm(extractionRoot, { recursive: true, force: true });
  }
}

function hostTriple() {
  const output = execFileSync('rustc', ['-Vv'], { encoding: 'utf8' });
  const line = output.split('\n').find((entry) => entry.startsWith('host:'));
  if (!line) throw new Error('[stage-conpty] 无法从 rustc -Vv 解析 host triple');
  return line.slice('host:'.length).trim();
}

function targetFromArgs() {
  const args = process.argv.slice(2);
  const targetIndex = args.indexOf('--target');
  return targetIndex === -1 ? hostTriple() : args[targetIndex + 1];
}

const invokedPath = process.argv[1]
  ? pathToFileURL(fileURLToPath(pathToFileURL(process.argv[1]))).href
  : null;
if (invokedPath === import.meta.url) {
  await stagePortableConpty({ target: targetFromArgs() });
}

