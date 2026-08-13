#!/usr/bin/env node
/**
 * 一键出正式包：npm run package
 *
 * 1. npm run tauri build —— pretauri 自动 stage sidecar，beforeBuildCommand
 *    出前端（tsc + vite build），随后 cargo release 编译并打安装包
 * 2. 从 src-tauri/target/release/bundle 收集安装包到仓库根 release/ 下，
 *    文件名带版本号，覆盖同名旧包
 *
 * 只想重新收集现成产物（构建已跑过）：node scripts/release-build.mjs --collect-only
 */
import { spawnSync } from 'node:child_process';
import { copyFileSync, mkdirSync, readFileSync, readdirSync, statSync } from 'node:fs';
import path from 'node:path';
import process from 'node:process';

const root = path.resolve(path.dirname(new URL(import.meta.url).pathname), '..');
const bundleDir = path.join(root, 'src-tauri', 'target', 'release', 'bundle');
const outDir = path.join(root, 'release');
const version = JSON.parse(readFileSync(path.join(root, 'package.json'), 'utf8')).version;

// 安装包后缀白名单：.app 是目录且被 .dmg 包含，不单独收集
const ARTIFACT_EXT = new Set(['.dmg', '.msi', '.exe', '.appimage', '.deb', '.rpm']);

if (!process.argv.includes('--collect-only')) {
  console.log(`[release-build] v${version} 开始构建（npm run tauri build）...`);
  const r = spawnSync('npm', ['run', 'tauri', 'build'], {
    cwd: root,
    stdio: 'inherit',
    // Windows 下 npm 是 npm.cmd，spawn 需经 shell 解析
    shell: process.platform === 'win32',
  });
  if (r.status !== 0) {
    console.error(`[release-build] 构建失败（exit ${r.status ?? 'signal'}）`);
    process.exit(r.status ?? 1);
  }
}

function walk(dir) {
  let files = [];
  for (const name of readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, name.name);
    // .app 之类的目录型 bundle 不进入（其内部文件后缀会误中白名单）
    if (name.isDirectory()) {
      if (!name.name.endsWith('.app')) files = files.concat(walk(p));
    } else if (ARTIFACT_EXT.has(path.extname(name.name).toLowerCase())) {
      files.push(p);
    }
  }
  return files;
}

let artifacts;
try {
  // 只认文件名带当前版本号的包：tauri 产物命名一贯含版本
  // （Mini-Term_0.11.3_aarch64.dmg / _x64-setup.exe / .msi），旧版本遗留
  // 与 create-dmg 中断留下的 rw.*.dmg 中间镜像借此一并排除
  artifacts = walk(bundleDir).filter((p) => path.basename(p).includes(version));
} catch {
  console.error(`[release-build] 未找到 ${bundleDir}，请先完整跑一次构建`);
  process.exit(1);
}
if (artifacts.length === 0) {
  console.error(`[release-build] ${bundleDir} 下没有 v${version} 的安装包产物`);
  process.exit(1);
}

mkdirSync(outDir, { recursive: true });
console.log(`\n[release-build] v${version} 产物：`);
for (const src of artifacts) {
  const dest = path.join(outDir, path.basename(src));
  copyFileSync(src, dest);
  const mb = (statSync(dest).size / 1024 / 1024).toFixed(1);
  console.log(`  ${path.relative(root, dest)}  (${mb} MB)`);
}
console.log('[release-build] 完成');
