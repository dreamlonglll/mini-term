#!/usr/bin/env node
/**
 * 字典转换器：src/i18n/locales/<ns>.ts  →  crates/mt-i18n/src/dict.rs
 *
 * 为什么是脚本而不是手抄：字典有 1000+ 条，手抄必然漏。脚本留在仓库里，
 * 之后 TS 侧（迁移期两套并存）再改文案，重跑一次即可，不用人肉对账。
 *
 * 工作原理：
 *   1. 每个 `<ns>.ts` 都是 `export const <ns> = { zh: {...}, en: {...} } as const;`，
 *      值全是字符串字面量（已核查：无模板串、无函数、无非字符串值）。
 *      把 `export const X =` 与结尾 `as const;` 剥掉，剩下的是**合法 JS 对象字面量**，
 *      交给 `new Function('return ...')` 求值即可拿到真实对象 —— 不需要 TS 编译器。
 *   2. 递归拍平成点分 key（`titleBar.status.error`），与 TS 侧 `t('app.titleBar.status.error')`
 *      去掉命名空间前缀后的部分完全一致。
 *   3. 按 key 排序后生成 Rust 静态表（运行时二分查找），并把 zh/en 条数写成常量，
 *      由 tests/consistency.rs 断言，防后续漂移。
 *
 * 用法（仓库根目录）：node crates/mt-i18n/tools/gen_from_ts.mjs
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, '..', '..', '..');
const localesDir = path.join(repoRoot, 'src', 'i18n', 'locales');
const outFile = path.join(repoRoot, 'crates', 'mt-i18n', 'src', 'dict.rs');

/** 从 `<ns>.ts` 源码里抠出对象字面量并求值 */
function evalNsFile(src, file) {
  const m = src.match(/export\s+const\s+([A-Za-z0-9_]+)\s*=\s*/);
  if (!m) throw new Error(`${file}: 找不到 export const 声明`);
  const objStart = src.indexOf('{', m.index + m[0].length);
  if (objStart < 0) throw new Error(`${file}: 找不到对象字面量起始 {`);
  // 结尾统一是 `} as const;`（已核查全部 32 个文件），取最后一个 `}`。
  const objEnd = src.lastIndexOf('}');
  const literal = src.slice(objStart, objEnd + 1);
  const value = new Function(`"use strict"; return (${literal});`)();
  return { name: m[1], value };
}

/** 递归拍平：{a:{b:"x"}} → {"a.b":"x"} */
function flatten(obj, prefix, out) {
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (typeof v === 'string') {
      out.set(key, v);
    } else if (v && typeof v === 'object') {
      flatten(v, key, out);
    } else {
      throw new Error(`非字符串叶子节点：${key} = ${JSON.stringify(v)}`);
    }
  }
  return out;
}

/** 取出 {name} 形式的占位符集合（与 TS 侧 store.ts 的 /\{(\w+)\}/g 同正则） */
function placeholders(s) {
  return new Set([...s.matchAll(/\{(\w+)\}/g)].map((m) => m[1]));
}

/** JS 字符串 → Rust 字符串字面量（非 ASCII 原样保留，源文件是 UTF-8） */
function rustStr(s) {
  let out = '"';
  for (const ch of s) {
    const c = ch.codePointAt(0);
    if (ch === '\\') out += '\\\\';
    else if (ch === '"') out += '\\"';
    else if (ch === '\n') out += '\\n';
    else if (ch === '\r') out += '\\r';
    else if (ch === '\t') out += '\\t';
    else if (c < 0x20 || c === 0x7f) out += `\\u{${c.toString(16)}}`;
    else out += ch;
  }
  return out + '"';
}

// ---------------------------------------------------------------------------
// 1. 读取全部命名空间
// ---------------------------------------------------------------------------
const files = fs
  .readdirSync(localesDir)
  .filter((f) => f.endsWith('.ts') && f !== 'index.ts')
  .sort();

// index.ts 是聚合入口，用它核对「文件都被收编了」，避免有孤儿字典文件被漏掉
const indexSrc = fs.readFileSync(path.join(localesDir, 'index.ts'), 'utf8');
const imported = new Set(
  [...indexSrc.matchAll(/^import\s*\{\s*([A-Za-z0-9_]+)\s*\}\s*from\s*'\.\/[^']+';/gm)].map(
    (m) => m[1],
  ),
);

const namespaces = [];
const report = { gaps: [], placeholderMismatch: [], empty: [] };
let zhTotal = 0;
let enTotal = 0;

for (const file of files) {
  const src = fs.readFileSync(path.join(localesDir, file), 'utf8');
  const { name, value } = evalNsFile(src, file);
  if (!imported.has(name)) {
    console.warn(`⚠ ${file}: 命名空间 ${name} 未被 locales/index.ts 收编，仍照常转换`);
  }
  const zh = flatten(value.zh ?? {}, '', new Map());
  const en = flatten(value.en ?? {}, '', new Map());

  // 差异体检
  for (const k of zh.keys()) if (!en.has(k)) report.gaps.push(`${name}.${k}  (en 缺失)`);
  for (const k of en.keys()) if (!zh.has(k)) report.gaps.push(`${name}.${k}  (zh 缺失)`);
  for (const [k, zv] of zh) {
    const ev = en.get(k);
    if (ev === undefined) continue;
    const a = [...placeholders(zv)].sort().join(',');
    const b = [...placeholders(ev)].sort().join(',');
    if (a !== b) report.placeholderMismatch.push(`${name}.${k}  zh={${a}} en={${b}}`);
  }
  for (const [k, v] of zh) if (v.trim() === '') report.empty.push(`zh ${name}.${k}`);
  for (const [k, v] of en) if (v.trim() === '') report.empty.push(`en ${name}.${k}`);

  zhTotal += zh.size;
  enTotal += en.size;
  namespaces.push({
    name,
    zh: [...zh.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)),
    en: [...en.entries()].sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)),
  });
}

namespaces.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));

// ---------------------------------------------------------------------------
// 2. 生成 Rust
// ---------------------------------------------------------------------------
const lines = [];
lines.push('//! 双语字典静态数据 —— 由 `tools/gen_from_ts.mjs` 从 `src/i18n/locales/*.ts` 生成。');
lines.push('//!');
lines.push('//! **不要手改本文件**：文案改动请改 TS 侧字典后重跑生成器');
lines.push('//! （`node crates/mt-i18n/tools/gen_from_ts.mjs`）。');
lines.push('//! Wave 3.5 之后 TS 侧删除时，本文件转为唯一真源，生成器随之退休。');
lines.push('//!');
lines.push('//! 数据布局：命名空间按 name 排序，每个命名空间内 key 按字典序排序，');
lines.push('//! 运行时一律二分查找 —— 零依赖、全部进 rodata、无初始化开销。');
lines.push('');
lines.push('use crate::Namespace;');
lines.push('');
lines.push('/// 命名空间总数（生成器对账用，测试断言防漂移）');
lines.push(`pub const NAMESPACE_COUNT: usize = ${namespaces.length};`);
lines.push('/// 中文条目总数');
lines.push(`pub const ZH_ENTRY_COUNT: usize = ${zhTotal};`);
lines.push('/// 英文条目总数');
lines.push(`pub const EN_ENTRY_COUNT: usize = ${enTotal};`);
lines.push('');

for (const ns of namespaces) {
  const upper = ns.name.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toUpperCase();
  for (const [lang, entries] of [
    ['ZH', ns.zh],
    ['EN', ns.en],
  ]) {
    // 一条文案一行：rustfmt 会把超长的 (key, value) 折成四行，
    // 之后改一个字的 diff 就变成整块重排，翻译改动没法审 —— 直接跳过格式化。
    lines.push('#[rustfmt::skip]');
    lines.push(`static ${upper}_${lang}: &[(&str, &str)] = &[`);
    for (const [k, v] of entries) lines.push(`    (${rustStr(k)}, ${rustStr(v)}),`);
    lines.push('];');
  }
  lines.push('');
}

lines.push('/// 全部命名空间，按 `name` 升序（`t` 靠这个顺序做二分查找）');
lines.push('pub static NAMESPACES: &[Namespace] = &[');
for (const ns of namespaces) {
  const upper = ns.name.replace(/([a-z0-9])([A-Z])/g, '$1_$2').toUpperCase();
  lines.push('    Namespace {');
  lines.push(`        name: ${rustStr(ns.name)},`);
  lines.push(`        zh: ${upper}_ZH,`);
  lines.push(`        en: ${upper}_EN,`);
  lines.push('    },');
}
lines.push('];');
lines.push('');

fs.writeFileSync(outFile, lines.join('\n'), 'utf8');

// ---------------------------------------------------------------------------
// 3. 对账报告
// ---------------------------------------------------------------------------
console.log(`命名空间 ${namespaces.length} 个；zh ${zhTotal} 条 / en ${enTotal} 条`);
console.log(`已写出 ${path.relative(repoRoot, outFile)}`);
if (report.gaps.length) {
  console.log(`\n两语言 key 差异 ${report.gaps.length} 处：`);
  for (const g of report.gaps) console.log('  ' + g);
} else {
  console.log('两语言 key 集合完全一致');
}
if (report.placeholderMismatch.length) {
  console.log(`\n占位符不一致 ${report.placeholderMismatch.length} 处：`);
  for (const g of report.placeholderMismatch) console.log('  ' + g);
} else {
  console.log('占位符两语言一致');
}
if (report.empty.length) {
  console.log(`\n空文案 ${report.empty.length} 处：`);
  for (const g of report.empty) console.log('  ' + g);
} else {
  console.log('无空文案');
}
