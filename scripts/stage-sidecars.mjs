// 构建 mini-term 的 sidecar 二进制（miniterm-hook、mt-ssh-mcp）并就位。
//
// 两个 sidecar 在独立 crate src-tauri/mt-sidecars/（不依赖 tauri-build），单独
// 构建不触发主程序的 externalBin 校验。tauri.conf.json 声明了 bundle.externalBin，
// tauri dev / tauri build 都校验 src-tauri/binaries/<name>-<triple>[.exe] 存在，
// 缺则失败 —— 故本脚本必产出这些文件。
//
// dev 经 package.json 的 pretauri 钩子调本脚本；CI（release.yml）直接调并带
// --release --target —— tauri-action 不走 npm run tauri，不会触发 pretauri。
//
//   node scripts/stage-sidecars.mjs                       dev: debug，triple 取 rustc host
//   node scripts/stage-sidecars.mjs --release --target T  CI:  release，triple = T

import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';

const SIDECARS = ['miniterm-hook', 'mt-ssh-mcp'];
const MANIFEST = join('src-tauri', 'mt-sidecars', 'Cargo.toml');
// externalBin 校验 + 发布打包都从这里取（文件名带 triple 后缀）。
const EXTERNAL_BIN_DIR = join('src-tauri', 'binaries');
// dev 下主程序跑在 src-tauri/target/debug/，sidecar 路径由运行时
// current_exe().parent() 解析，故 debug sidecar 也要落在这里（裸名）。
const DEV_EXE_DIR = join('src-tauri', 'target', 'debug');

const args = process.argv.slice(2);
const release = args.includes('--release');
const ti = args.indexOf('--target');
const explicitTarget = ti !== -1 ? args[ti + 1] : null;

// CI 用显式 --target；dev 取 rustc 自报的 host triple（按 platform/arch 手拼易错）。
function hostTriple() {
  const out = execFileSync('rustc', ['-Vv'], { encoding: 'utf8' });
  const line = out.split('\n').find((l) => l.startsWith('host:'));
  if (!line) throw new Error('无法从 rustc -Vv 解析 host triple');
  return line.slice('host:'.length).trim();
}

const triple = explicitTarget ?? hostTriple();
const profile = release ? 'release' : 'debug';
const ext = triple.includes('windows') ? '.exe' : '';

const cargoArgs = ['build', '--manifest-path', MANIFEST];
if (release) cargoArgs.push('--release');
if (explicitTarget) cargoArgs.push('--target', explicitTarget);
for (const name of SIDECARS) cargoArgs.push('--bin', name);

console.log(`[stage-sidecars] triple=${triple} profile=${profile}`);
console.log(`[stage-sidecars] cargo ${cargoArgs.join(' ')}`);
execFileSync('cargo', cargoArgs, { stdio: 'inherit' });

// 带 --target 时产物在 target/<triple>/<profile>/，否则 target/<profile>/。
const builtDir = explicitTarget
  ? join('src-tauri', 'mt-sidecars', 'target', explicitTarget, profile)
  : join('src-tauri', 'mt-sidecars', 'target', profile);

mkdirSync(EXTERNAL_BIN_DIR, { recursive: true });
if (!release) mkdirSync(DEV_EXE_DIR, { recursive: true });

for (const name of SIDECARS) {
  const from = join(builtDir, `${name}${ext}`);

  const staged = join(EXTERNAL_BIN_DIR, `${name}-${triple}${ext}`);
  copyFileSync(from, staged);
  console.log(`[stage-sidecars] ${from} -> ${staged}`);

  // dev：再放一份裸名到主程序同目录，供运行时 current_exe().parent() 定位。
  // 该文件可能正被运行中的 MCP server 占用而无法覆盖 —— 跳过即可（旧副本仍在，
  // tauri dev 照常起；要换新版本需重启该 MCP server）。
  if (!release) {
    const devCopy = join(DEV_EXE_DIR, `${name}${ext}`);
    try {
      copyFileSync(from, devCopy);
      console.log(`[stage-sidecars] ${from} -> ${devCopy}`);
    } catch (e) {
      console.warn(`[stage-sidecars] 跳过 ${devCopy}（可能正在运行）: ${e.code ?? e.message}`);
    }
  }
}
console.log('[stage-sidecars] done');
