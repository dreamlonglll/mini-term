// 列出主工作区里**不依赖 gpui** 的 crate,按 `-p <name>` 的形式逐个 token 一行打印,
// 给 Windows 作业的 `cargo test` 当参数用:
//
//   pkgs="$(node .github/scripts/non_gpui_packages.mjs)"
//   cargo test $pkgs
//
// 为什么要排除 gpui:gpui(gpui-fast)的依赖树是整个工作区编译量的大头,Windows 作业
// 只为覆盖 `cfg(windows)` 的单测,不值得为 mt-app / mt-ui 的测试二进制再编一遍它
// (它们的 Windows 代码由同一作业的 `cargo check` / clippy 覆盖)。
//
// 判定口径:成员自身的 normal / build / dev 依赖 + 传递依赖的 normal / build 依赖
// (依赖的 dev 依赖不参与编译),全平台(不加 --filter-platform),闭包里出现名为
// `gpui-fast` 的包即算依赖 gpui。新增 crate 自动归类,不用回来改名单。
//
// ci.yml(消费方)与 cache-warm.yml(预热方)都走这一份:`-p` 集合不同会让 feature
// 统一的结果不同,预热出来的依赖产物就对不上。

import { execFileSync } from 'node:child_process';

const GPUI_PACKAGE = 'gpui-fast';

const meta = JSON.parse(
  execFileSync('cargo', ['metadata', '--format-version', '1'], {
    encoding: 'utf8',
    maxBuffer: 256 * 1024 * 1024,
  }),
);

const nameOf = new Map(meta.packages.map((p) => [p.id, p.name]));
const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));

// 依赖边:kinds 里任一种在允许集合内即算(dep_kinds 的 kind 为 null 表示 normal)
function depsOf(id, allowDev) {
  const node = nodes.get(id);
  if (!node) return [];
  return node.deps
    .filter((d) =>
      d.dep_kinds.some((k) => k.kind === null || k.kind === 'build' || (allowDev && k.kind === 'dev')),
    )
    .map((d) => d.pkg);
}

function dependsOnGpui(memberId) {
  const seen = new Set();
  const stack = depsOf(memberId, true);
  while (stack.length) {
    const id = stack.pop();
    if (seen.has(id)) continue;
    seen.add(id);
    if (nameOf.get(id) === GPUI_PACKAGE) return true;
    stack.push(...depsOf(id, false));
  }
  return false;
}

const picked = meta.workspace_members
  .filter((id) => !dependsOnGpui(id))
  .map((id) => nameOf.get(id))
  .sort();
if (!picked.length) {
  console.error('::error::没有找到不依赖 gpui 的 crate —— 判定逻辑或工作区结构变了');
  process.exit(1);
}
console.error(`non-gpui crates (${picked.length}): ${picked.join(' ')}`);
for (const name of picked) console.log(`-p\n${name}`);
