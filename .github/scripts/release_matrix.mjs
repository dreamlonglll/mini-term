// 算出 release.yml 里 build-gpui 的矩阵(strategy.matrix.include 数组)。
//
//   EVENT_NAME=push node .github/scripts/release_matrix.mjs
//   EVENT_NAME=workflow_dispatch PLATFORMS="macos-x64, linux" node .github/scripts/release_matrix.mjs
//
// push(推 v* tag 发版)是全量四条;workflow_dispatch(给已发布版本补建产物)只留
// PLATFORMS 点名的那些,key 写错直接报错而不是静默漏掉。job 级 if 拿不到 matrix
// 上下文,所以过滤只能在 plan 作业里做完、再 fromJSON 喂进 strategy.matrix。
//
// `arch` 只有 macOS 用(dmg 资产名的架构段:Apple Silicon 沿用旧 Tauri 版的
// aarch64,Intel 与 Windows 安装包同一套叫 x64);Windows / Linux 的资产名写死在
// release.yml 各自的组装步骤里。
//
// 结果写进 $GITHUB_OUTPUT 的 include=<json>;没有该环境变量(本地试跑)只打印。

import { appendFileSync } from 'node:fs';

const ALL = [
  { key: 'windows', platform: 'windows-latest', target: 'x86_64-pc-windows-msvc' },
  { key: 'macos-arm64', platform: 'macos-latest', target: 'aarch64-apple-darwin', arch: 'aarch64' },
  { key: 'macos-x64', platform: 'macos-15-intel', target: 'x86_64-apple-darwin', arch: 'x64' },
  { key: 'linux', platform: 'ubuntu-22.04', target: 'x86_64-unknown-linux-gnu' },
];

function planMatrix(eventName, platforms) {
  if (eventName === 'push') return ALL;
  const keys = (platforms ?? '')
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean);
  const known = ALL.map((e) => e.key);
  const unknown = keys.filter((k) => !known.includes(k));
  if (unknown.length) {
    throw new Error(`未知平台 key: ${unknown.join(', ')}(可选 ${known.join(' / ')})`);
  }
  const picked = ALL.filter((e) => keys.includes(e.key));
  if (!picked.length) throw new Error(`platforms 为空,没有要构建的平台(可选 ${known.join(' / ')})`);
  return picked;
}

let include;
try {
  include = planMatrix(process.env.EVENT_NAME, process.env.PLATFORMS);
} catch (e) {
  console.error(`::error::${e.message}`);
  process.exit(1);
}
const json = JSON.stringify(include);
console.log(`matrix include: ${json}`);
if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `include=${json}\n`);
