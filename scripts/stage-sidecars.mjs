// 构建 mini-term 的 sidecar 二进制（miniterm-hook、mt-ssh-mcp、mt-ssh-cli、
// mt-agent-cli）并就位。
//
// 这些 sidecar 在独立工作区 sidecars/（版本号自成语义，不并入根 workspace），
// 运行时由主程序按 current_exe().parent() 同目录裸名定位 —— 本脚本把产物
// 拷进主程序所在的 target/<profile>/，dev 与 release 同一套就位模型；
// Windows 下再把便携 ConPTY 一并就位到同目录的 portable-conpty/。
//
//   node scripts/stage-sidecars.mjs                       dev: debug，triple 取 rustc host
//   node scripts/stage-sidecars.mjs --release --target T  CI:  release，triple = T
//
// release.yml 的便携包组装步骤直接从 target/release/ 收集 exe、sidecar 与
// portable-conpty/ —— 目录布局即最终 zip 布局。

import { execFileSync } from 'node:child_process';
import { copyFileSync, mkdirSync } from 'node:fs';
import { join } from 'node:path';
import { stagePortableConpty, WINDOWS_X64_TARGET } from './stage-conpty.mjs';

// mt-agent-cli 是编排者的控制 CLI（ADR 0003）：编排者 pane 里的 agent 靠它
// 调主程序的控制端点。与另外三个同一套「与 exe 同目录裸名定位」的模型。
const SIDECARS = ['miniterm-hook', 'mt-ssh-mcp', 'mt-ssh-cli', 'mt-agent-cli'];
const MANIFEST = join('sidecars', 'Cargo.toml');

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

// 主程序（cargo build -p mt-app，不带 --target）的产物目录 —— sidecar 与
// portable-conpty 都就位到这里，与运行时 exe 同目录解析对齐。
const EXE_DIR = join('target', profile);

const cargoArgs = ['build', '--manifest-path', MANIFEST];
if (release) cargoArgs.push('--release');
if (explicitTarget) cargoArgs.push('--target', explicitTarget);
for (const name of SIDECARS) cargoArgs.push('--bin', name);

console.log(`[stage-sidecars] triple=${triple} profile=${profile}`);
console.log(`[stage-sidecars] cargo ${cargoArgs.join(' ')}`);
execFileSync('cargo', cargoArgs, { stdio: 'inherit' });

// 带 --target 时产物在 target/<triple>/<profile>/，否则 target/<profile>/。
const builtDir = explicitTarget
  ? join('sidecars', 'target', explicitTarget, profile)
  : join('sidecars', 'target', profile);

mkdirSync(EXE_DIR, { recursive: true });

for (const name of SIDECARS) {
  const from = join(builtDir, `${name}${ext}`);
  const staged = join(EXE_DIR, `${name}${ext}`);
  // 目标文件可能正被运行中的 MCP server / daemon 占用而无法覆盖 —— 跳过即可
  // （旧副本仍在，主程序照常起；要换新版本需先重启占用方）。
  try {
    copyFileSync(from, staged);
    console.log(`[stage-sidecars] ${from} -> ${staged}`);
  } catch (e) {
    if (release) throw e;
    console.warn(`[stage-sidecars] 跳过 ${staged}（可能正在运行）: ${e.code ?? e.message}`);
  }
}

if (triple.includes('windows')) {
  if (triple !== WINDOWS_X64_TARGET) {
    throw new Error(
      `[stage-sidecars] Windows 目标 ${triple} 尚无匹配的便携 ConPTY 资源`,
    );
  }
  await stagePortableConpty({
    target: triple,
    outputDir: join(EXE_DIR, 'portable-conpty'),
  });
} else {
  console.log(`[stage-sidecars] ${triple} 非 Windows，跳过便携 ConPTY staging`);
}
console.log('[stage-sidecars] done');
