#!/usr/bin/env node
/**
 * 技术栈徽标生成器：devicon → crates/mt-ui/src/icons/tech_art.rs
 *
 * 原版（Tauri/React）用的就是 devicon 的 `*-original.svg` 裸资产；改造前的 GPUI 版
 * 拿不到源文件，只能自绘几何骨架 + 官方品牌色，自己在注释里承认「Java / Python / Go /
 * PHP / Svelte / Next.js 退成品牌色片 + 简化字形，认色不认形」。这里把官方 logo 的
 * 那条 `d` 原样烘焙进来，认色也认形，顺带把种类从 12 扩到 50。
 *
 * 展平管线与文件图标共用 `icon_pipeline.mjs`。
 *
 * ## 用法
 *
 * ```bash
 * cd crates/mt-ui/tools && npm install
 * node gen_tech_icons.mjs            # 覆写 ../src/icons/tech_art.rs
 * node gen_tech_icons.mjs --preview  # 另出 target/tech-icons-preview.html
 * ```
 *
 * 产物是**生成物，禁止手改** —— 连 `ProjectKind` 枚举本身都由下面的 CATALOG 生成，
 * 改种类只改 CATALOG 后重跑。
 *
 * ## 深色底可读性
 *
 * devicon 里有一批 logo 官方就是纯黑/近黑（Rust、Apple、Express、Remix…），画在
 * 本应用的深色面板上等于隐形。生成器按 WCAG 对比度自动检测，再分两档处理：
 * **单色**的整枚改成跟随主题色（`Ink::Current`，与 `brand.rs` 对「官方本为黑白的
 * 品牌」同一口径，浅色主题下会自动变回深色）；**多色**的逐笔保色相提亮
 * —— 压成一个色会把内部层次糊成一团。
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import {
  preprocess, flatten, rebuildSvg, fingerprint, rustIdent, emitShapes,
} from './icon_pipeline.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const deviconDir = path.resolve(here, 'node_modules', 'devicon', 'icons');
const outFile = path.resolve(here, '..', 'src', 'icons', 'tech_art.rs');
const previewFile = path.resolve(here, '..', '..', '..', 'target', 'tech-icons-preview.html');

/**
 * 项目类型总表 —— **唯一真相**，`ProjectKind` 枚举、菜单顺序、图标全从这里生成。
 *
 * 每项 `[kind, variant, devicon, label, category]`：
 * - `kind`：落盘字符串。用户手动指定的类型会写进配置（`kindOverride`），
 *   **已有的十二个一个字都不能改**（java / rust / go / python / flutter / php /
 *   vuejs / nextjs / react / svelte / vite / nodejs），否则存量配置读不回来；
 * - `variant`：Rust 枚举变体名；
 * - `devicon`：devicon 的图标目录名（变体自动挑，优先 original）；
 * - `label`：菜单显示名（专有名词，不进 i18n）；
 * - `category`：菜单里的二级分组。
 */
export const CATALOG = [
  // ── 语言 ──
  ['rust', 'Rust', 'rust', 'Rust', 'Language'],
  ['go', 'Go', 'go', 'Go', 'Language'],
  ['java', 'Java', 'java', 'Java', 'Language'],
  ['kotlin', 'Kotlin', 'kotlin', 'Kotlin', 'Language'],
  ['python', 'Python', 'python', 'Python', 'Language'],
  ['csharp', 'CSharp', 'csharp', 'C#', 'Language'],
  ['cpp', 'Cpp', 'cplusplus', 'C++', 'Language'],
  ['c', 'C', 'c', 'C', 'Language'],
  ['swift', 'Swift', 'swift', 'Swift', 'Language'],
  ['ruby', 'Ruby', 'ruby', 'Ruby', 'Language'],
  ['php', 'Php', 'php', 'PHP', 'Language'],
  ['dart', 'Dart', 'dart', 'Dart', 'Language'],
  ['elixir', 'Elixir', 'elixir', 'Elixir', 'Language'],
  ['scala', 'Scala', 'scala', 'Scala', 'Language'],
  ['haskell', 'Haskell', 'haskell', 'Haskell', 'Language'],
  ['zig', 'Zig', 'zig', 'Zig', 'Language'],
  ['lua', 'Lua', 'lua', 'Lua', 'Language'],
  ['perl', 'Perl', 'perl', 'Perl', 'Language'],
  // ── 前端 ──
  ['react', 'React', 'react', 'React', 'Frontend'],
  ['vuejs', 'Vue', 'vuejs', 'Vue', 'Frontend'],
  ['angular', 'Angular', 'angular', 'Angular', 'Frontend'],
  ['svelte', 'Svelte', 'svelte', 'Svelte', 'Frontend'],
  ['nextjs', 'Next', 'nextjs', 'Next.js', 'Frontend'],
  ['nuxtjs', 'Nuxt', 'nuxtjs', 'Nuxt', 'Frontend'],
  ['astro', 'Astro', 'astro', 'Astro', 'Frontend'],
  ['solidjs', 'Solid', 'solidjs', 'Solid', 'Frontend'],
  ['remix', 'Remix', 'remix', 'Remix', 'Frontend'],
  ['vite', 'Vite', 'vitejs', 'Vite', 'Frontend'],
  ['nodejs', 'Node', 'nodejs', 'Node.js', 'Frontend'],
  // ── 后端 ──
  ['django', 'Django', 'django', 'Django', 'Backend'],
  ['flask', 'Flask', 'flask', 'Flask', 'Backend'],
  ['fastapi', 'FastApi', 'fastapi', 'FastAPI', 'Backend'],
  ['rails', 'Rails', 'rails', 'Ruby on Rails', 'Backend'],
  ['laravel', 'Laravel', 'laravel', 'Laravel', 'Backend'],
  ['spring', 'Spring', 'spring', 'Spring', 'Backend'],
  ['dotnet', 'DotNet', 'dotnetcore', '.NET', 'Backend'],
  ['nestjs', 'Nest', 'nestjs', 'NestJS', 'Backend'],
  ['express', 'Express', 'express', 'Express', 'Backend'],
  ['deno', 'Deno', 'denojs', 'Deno', 'Backend'],
  ['bun', 'Bun', 'bun', 'Bun', 'Backend'],
  // ── 移动与跨端 ──
  ['flutter', 'Flutter', 'flutter', 'Flutter', 'Mobile'],
  ['android', 'Android', 'android', 'Android', 'Mobile'],
  ['apple', 'Apple', 'apple', 'iOS / macOS', 'Mobile'],
  ['tauri', 'Tauri', 'tauri', 'Tauri', 'Mobile'],
  ['electron', 'Electron', 'electron', 'Electron', 'Mobile'],
  ['unity', 'Unity', 'unity', 'Unity', 'Mobile'],
  ['godot', 'Godot', 'godot', 'Godot', 'Mobile'],
  // ── 基础设施 ──
  ['docker', 'Docker', 'docker', 'Docker', 'Infra'],
  ['kubernetes', 'Kubernetes', 'kubernetes', 'Kubernetes', 'Infra'],
  ['terraform', 'Terraform', 'terraform', 'Terraform', 'Infra'],
  ['ansible', 'Ansible', 'ansible', 'Ansible', 'Infra'],
];

/**
 * 变体优先级：要的是「有配色的完整 logo」，退而求其次才是单色描边。
 *
 * `*-wordmark` 一概不用 —— 那是 logo + 品牌文字的横排版式，14px 下文字必糊成一团，
 * 且宽高比远离 1:1，塞进正方形画布会缩到只剩一点点。
 */
const VARIANTS = ['original', 'plain', 'line'];

/** 面板底色（`--bg-elevated`）。判「这枚 logo 在深色底上看不看得见」的参照。 */
const PANEL_BG = [0x1c, 0x1a, 0x18];

/** WCAG 相对亮度。 */
function relativeLuminance([r, g, b]) {
  const lin = (c) => {
    const s = c / 255;
    return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

/** 与面板底色的 WCAG 对比度（1..21）。 */
function contrastWithPanel(color) {
  const [a, b] = [relativeLuminance(color), relativeLuminance(PANEL_BG)];
  const [hi, lo] = a > b ? [a, b] : [b, a];
  return (hi + 0.05) / (lo + 0.05);
}

/**
 * 一枚 logo 在深色底上是不是基本看不见。
 *
 * devicon 里 Rust / Apple / Express / Deno / Remix 这些官方就是纯黑，画在深色面板上
 * 等于隐形。判据是 **WCAG 对比度**而不是绝对亮度：饱和的深红（Rails 的 #d30001）
 * 亮度很低但对深色底有 3:1 的对比、看得清清楚楚，按亮度判会把它一起误伤成白色。
 *
 * 阈值取 2.5（WCAG 对非文本图形要求 3:1，这里略放宽，宁可保留官方配色）。
 * 按 `d` 的长度加权：路径越长通常铺的面积越大，只有一两笔黑描边不算「整枚都暗」。
 */
export function tooDarkForDarkUi(shapes) {
  let lit = 0;
  let total = 0;
  for (const s of shapes) {
    if (s.color === 'current') return false;
    const weight = s.d.length;
    total += weight;
    if (contrastWithPanel(s.color) >= 2.5) lit += weight;
  }
  return total > 0 && lit / total < 0.12;
}

function rgbToHsl([r, g, b]) {
  const [R, G, B] = [r / 255, g / 255, b / 255];
  const max = Math.max(R, G, B);
  const min = Math.min(R, G, B);
  const l = (max + min) / 2;
  if (max === min) return [0, 0, l];
  const d = max - min;
  const s = l > 0.5 ? d / (2 - max - min) : d / (max + min);
  let h;
  if (max === R) h = ((G - B) / d + (G < B ? 6 : 0)) / 6;
  else if (max === G) h = ((B - R) / d + 2) / 6;
  else h = ((R - G) / d + 4) / 6;
  return [h, s, l];
}

function hslToRgb([h, s, l]) {
  if (s === 0) return [l, l, l].map((v) => Math.round(v * 255));
  const q = l < 0.5 ? l * (1 + s) : l + s - l * s;
  const p = 2 * l - q;
  const hue = (t) => {
    let x = t;
    if (x < 0) x += 1;
    if (x > 1) x -= 1;
    if (x < 1 / 6) return p + (q - p) * 6 * x;
    if (x < 1 / 2) return q;
    if (x < 2 / 3) return p + (q - p) * (2 / 3 - x) * 6;
    return p;
  };
  return [hue(h + 1 / 3), hue(h), hue(h - 1 / 3)].map((v) => Math.round(v * 255));
}

/**
 * 保色相地提亮到对深色底有 3:1 的对比。
 *
 * 给**多色**的暗 logo 用：.NET 的深紫、Perl 的深蓝洋葱，整枚压成一个色会把内部层次
 * 糊成一团白，提亮则既看得见又还认得出是哪家的配色。
 */
function lightenForDarkUi(color) {
  const [h, s] = rgbToHsl(color);
  let [, , l] = rgbToHsl(color);
  for (let i = 0; i < 60 && contrastWithPanel(hslToRgb([h, s, l])) < 3; i++) {
    l = Math.min(1, l + 0.02);
  }
  return hslToRgb([h, s, l]);
}

/**
 * 让一枚在深色底上看不见的 logo 变得可读，返回是否动过手。
 *
 * 两种手法，按**用了几种颜色**分：
 * - **单色**（Rust / Apple / Express / Remix / Django 这些官方就是一个黑）：整枚改成
 *   跟随主题色。它在浅色主题下会自动变回深色 —— 换成固定的浅灰就只在深色主题下好看；
 * - **多色**：逐笔保色相提亮。压成一个色会丢层次。
 */
export function makeReadable(shapes) {
  const distinct = new Set(shapes.map((s) => (s.color === 'current' ? 'c' : s.color.join(','))));
  if (distinct.size <= 1) {
    for (const s of shapes) s.color = 'current';
    return 'mono';
  }
  for (const s of shapes) {
    if (s.color !== 'current') s.color = lightenForDarkUi(s.color);
  }
  return 'lighten';
}

export function loadIcon(name) {
  const dir = path.join(deviconDir, name);
  if (!fs.existsSync(dir)) throw new Error(`devicon 里没有 ${name}/`);
  const files = fs.readdirSync(dir);
  for (const v of VARIANTS) {
    const file = `${name}-${v}.svg`;
    if (files.includes(file)) {
      return { variant: v, svg: preprocess(fs.readFileSync(path.join(dir, file), 'utf8')) };
    }
  }
  throw new Error(`${name}/ 里没有可用变体（有 ${files.join(' ')}）`);
}

// ─────────────────────────── Rust 代码生成 ───────────────────────────

function emitRust(entries) {
  const lines = [];
  const push = (s = '') => lines.push(s);

  push('//! 技术栈徽标与项目类型 —— **生成物，禁止手改**。');
  push('//!');
  push('//! 由 `crates/mt-ui/tools/gen_tech_icons.mjs` 从 devicon（MIT，与原版 Tauri 前端');
  push('//! 同一批资产）烘焙而来：官方 logo 的那条 `d` 原样搬进 [`Geom::Path`]，渲染仍是自绘');
  push('//! （判据见 [`super::vector`] 模块注释）。');
  push('//!');
  push('//! 连 [`ProjectKind`] 枚举本身都是生成的 —— 种类、落盘字符串、展示名、菜单分组');
  push('//! 四者必须同步，分开手写迟早对不上。改种类请改生成器的 CATALOG 后重跑。');
  push('//!');
  push('//! ⚠ [`ProjectKind::as_str`] 的取值会**落盘**（用户手动指定的类型存进配置的');
  push('//! `kindOverride`），已有取值一个字都不能改，否则存量配置读不回来。');
  push('//!');
  push('//! 商标注意（沿用 brand.rs 的红线）：logo 仅作「这个项目是什么技术栈」的指示性');
  push('//! 使用，不得用作产品自身标识。');
  push('');
  push('use super::tech::TechCategory;');
  push('use super::vector::{Geom, Ink, Shape};');
  push('');

  for (const e of entries) {
    const note = e.recolored === 'mono' ? '，官方为单色近黑、改跟随主题' : e.recolored === 'lighten' ? '，官方在深色底上偏暗、已保色相提亮' : '';
    push(`/// ${e.label} — devicon \`${e.devicon}-${e.variantPicked}\`${note}`);
    push(`static ${rustIdent(e.kind)}: &[Shape] = ${emitShapes(e)};`);
    push('');
  }

  push('/// 项目技术栈。取值来自生成器 CATALOG，顺序即菜单顺序（按分组聚拢）。');
  push('#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]');
  push('pub enum ProjectKind {');
  for (const e of entries) push(`    ${e.variant},`);
  push('}');
  push('');

  push('impl ProjectKind {');
  push('    /// **会落盘**的字符串（配置里的 `kindOverride`）。改一个字就读不回存量配置。');
  push('    pub fn as_str(self) -> &\'static str {');
  push('        match self {');
  for (const e of entries) push(`            Self::${e.variant} => "${e.kind}",`);
  push('        }');
  push('    }');
  push('');
  push('    // 与 `mt_app::tree::PaneStatus::from_str` 取同一个命名（返回 Option 而非 Result，');
  push('    // 失败没有可报的错误细节），不实现 `FromStr` trait');
  push('    #[allow(clippy::should_implement_trait)]');
  push('    pub fn from_str(s: &str) -> Option<Self> {');
  push('        Some(match s {');
  for (const e of entries) push(`            "${e.kind}" => Self::${e.variant},`);
  push('            _ => return None,');
  push('        })');
  push('    }');
  push('');
  push('    /// 展示名。专有名词，不进 i18n。');
  push('    pub fn label(self) -> &\'static str {');
  push('        match self {');
  for (const e of entries) push(`            Self::${e.variant} => "${e.label}",`);
  push('        }');
  push('    }');
  push('');
  push('    /// 菜单里归到哪个二级分组。');
  push('    pub fn category(self) -> TechCategory {');
  push('        match self {');
  for (const e of entries) push(`            Self::${e.variant} => TechCategory::${e.category},`);
  push('        }');
  push('    }');
  push('');
  push('    pub(super) fn shapes(self) -> &\'static [Shape] {');
  push('        match self {');
  for (const e of entries) push(`            Self::${e.variant} => ${rustIdent(e.kind)},`);
  push('        }');
  push('    }');
  push('}');
  push('');

  push('/// 全部种类，**已按分组聚拢**：菜单直接按这个顺序分段即可。');
  push('pub const ALL_PROJECT_KINDS: &[ProjectKind] = &[');
  for (const e of entries) push(`    ProjectKind::${e.variant},`);
  push('];');
  push('');

  return lines.join('\n');
}

function emitPreview(entries) {
  const cell = (e) => `
  <div class="cell">
    <div class="pair"><span title="官方原图">${e.original}</span><span title="展平后">${e.rebuilt}</span></div>
    <code>${e.label}${e.recolored ? ' ⚠改色' : ''}</code>
  </div>`;
  return `<!doctype html><meta charset="utf-8"><title>tech icons 验收</title>
<style>
 body{background:#1c1a18;color:#f0ece6;font:13px/1.5 ui-monospace,Consolas,monospace;margin:16px}
 h1{font-size:15px;font-weight:600}
 .grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(150px,1fr));gap:10px}
 .cell{background:#252220;border-radius:6px;padding:8px;text-align:center}
 .pair{display:flex;justify-content:center;gap:10px;align-items:center}
 .pair svg{width:34px;height:34px}
 .pair span:nth-child(2){border-left:1px solid #4a443f;padding-left:10px}
 code{display:block;margin-top:4px;color:#9aa7b0;font-size:11px}
</style>
<h1>左 = devicon 原图，右 = 展平重建（共 ${entries.length} 枚，深色底上验收）。</h1>
<div class="grid">${entries.map(cell).join('')}</div>`;
}

// ─────────────────────────── 主流程 ───────────────────────────

function main() {
  const wantPreview = process.argv.includes('--preview');
  const order = ['Language', 'Frontend', 'Backend', 'Mobile', 'Infra'];
  const entries = [];
  const warnings = [];
  const seen = new Map();

  for (const [kind, variant, devicon, label, category] of CATALOG) {
    const { variant: pickedVariant, svg } = loadIcon(devicon);
    const art = flatten(svg, kind);
    // 只改「墨水」，几何仍是官方的
    const recolored = tooDarkForDarkUi(art.shapes) ? makeReadable(art.shapes) : false;
    if (art.warnings.length) warnings.push({ kind, warnings: [...new Set(art.warnings)] });
    const fp = fingerprint(art);
    const dup = seen.get(fp);
    if (dup) warnings.push({ kind, warnings: [`与 ${dup} 是同一张图（devicon 名写错了？）`] });
    seen.set(fp, kind);
    entries.push({ ...art, kind, variant, devicon, label, category, recolored, variantPicked: pickedVariant });
    if (pickedVariant !== 'original') {
      warnings.push({ kind, warnings: [`没有 original 变体，用了 ${pickedVariant}`] });
    }
  }

  entries.sort((a, b) => order.indexOf(a.category) - order.indexOf(b.category));
  fs.writeFileSync(outFile, emitRust(entries));

  console.log(`✓ ${path.relative(process.cwd(), outFile)}`);
  console.log(`  技术栈 ${entries.length} 种：${order.map((c) => `${c} ${entries.filter((e) => e.category === c).length}`).join(' / ')}`);
  const mono = entries.filter((e) => e.recolored === 'mono').map((e) => e.label);
  const lit = entries.filter((e) => e.recolored === 'lighten').map((e) => e.label);
  console.log(`  深色底不可读 → 单色改跟随主题：${mono.join(' ') || '（无）'}`);
  console.log(`               → 多色保色相提亮：${lit.join(' ') || '（无）'}`);
  console.log(`  源码 ${(fs.statSync(outFile).size / 1024).toFixed(0)} KB`);
  if (warnings.length) {
    console.log(`\n⚠ ${warnings.length} 条降级/提示：`);
    for (const w of warnings) console.log(`  ${w.kind.padEnd(14)} ${w.warnings.join('；')}`);
  }

  if (wantPreview) {
    const withSvg = entries.map((e) => ({
      ...e,
      original: loadIcon(e.devicon).svg,
      rebuilt: rebuildSvg(e),
    }));
    fs.mkdirSync(path.dirname(previewFile), { recursive: true });
    fs.writeFileSync(previewFile, emitPreview(withSvg));
    console.log(`\n✓ 预览页 ${path.relative(process.cwd(), previewFile)}`);
  }
}

// 被 verify_icons.mjs import 时不要顺手把文件重写一遍
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}
