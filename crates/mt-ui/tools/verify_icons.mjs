#!/usr/bin/env node
/**
 * 图标烘焙的验收台：**同一个渲染器**分别渲染「官方原图」与「展平重建图」，逐像素比对。
 *
 * 管线丢了 transform、漏了一笔、颜色继承错了、clip 求交把洞填实了 —— 都会在这张榜上
 * 冒头，不必肉眼扫三百多枚。文件图标那批的三个真 bug 就是它抓出来的。
 *
 * ```bash
 * node verify_icons.mjs              # 两套都验
 * node verify_icons.mjs file         # 只验文件/目录图标
 * node verify_icons.mjs tech         # 只验技术栈徽标
 * node verify_icons.mjs tech --all   # 对照图里放全部（默认只放差异最大的 32 对）
 * ```
 *
 * 产物 `target/<file|tech>-icons-diff.png`（上=官方原图，下=展平重建）。
 *
 * 差异度是 0..1 的平均像素距离（含 alpha）。经验阈值：
 * `< 0.02` 肉眼看不出；`0.02..0.06` 多半是抗锯齿/曲线离散的正常抖动；
 * `> 0.06` 要人工看，通常意味着真丢了东西。
 *
 * 技术栈那批有一档**预期内的**大差异：devicon 里官方就是纯黑的 logo（Rails、Express、
 * Apple…）会被生成器改成跟随主题色，与原图比自然差得远。这类会标注「改色」，看形状即可。
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Resvg } from '@resvg/resvg-js';
import { flatten, rebuildSvg } from './icon_pipeline.mjs';
import {
  fetchIcon, EXACT_NAMES, NAME_PREFIXES, EXTENSIONS, FOLDER_NAMES,
} from './gen_file_icons.mjs';
import { CATALOG, loadIcon, tooDarkForDarkUi, makeReadable } from './gen_tech_icons.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const targetDir = path.resolve(here, '..', '..', '..', 'target');

/** 比对分辨率。比实际显示的 14px 大得多，好让小差异也能被抓到。 */
const SIZE = 64;

/**
 * viewBox 补成正方形（居中）。重建图的 view 本就是方的（`Geom::Path` 只吃方画布），
 * 原图不补的话两边画布高度不同，比对器会把「Docker 那枚 41×34.5」直接判成全错。
 */
function squarify(svg) {
  const m = svg.match(/viewBox\s*=\s*"([^"]+)"/);
  if (!m) return svg;
  const [x, y, w, h] = m[1].trim().split(/[\s,]+/).map(Number);
  if (!(w > 0 && h > 0) || Math.abs(w - h) < 1e-6) return svg;
  const side = Math.max(w, h);
  return svg.replace(m[0], `viewBox="${x - (side - w) / 2} ${y - (side - h) / 2} ${side} ${side}"`);
}

function render(svg, size = SIZE) {
  const r = new Resvg(squarify(svg), { fitTo: { mode: 'width', value: size }, background: 'rgba(0,0,0,0)' });
  const img = r.render();
  return { pixels: img.pixels, width: img.width, height: img.height };
}

/** 两张同尺寸 RGBA 位图的平均像素距离（0..1）。 */
function diff(a, b) {
  if (a.width !== b.width || a.height !== b.height) return 1;
  let acc = 0;
  for (let i = 0; i < a.pixels.length; i += 4) {
    // 按 alpha 加权比颜色：全透明处的颜色值没有意义，直接比会把噪声算进来
    const [aa, ba] = [a.pixels[i + 3] / 255, b.pixels[i + 3] / 255];
    let d = Math.abs(aa - ba);
    const w = Math.min(aa, ba);
    if (w > 0) {
      d += w * (Math.abs(a.pixels[i] - b.pixels[i]) + Math.abs(a.pixels[i + 1] - b.pixels[i + 1]) + Math.abs(a.pixels[i + 2] - b.pixels[i + 2])) / (3 * 255);
    }
    acc += Math.min(d, 1);
  }
  return acc / (a.pixels.length / 4);
}

// ─────────────────────────── 两套图标各自的取图 ───────────────────────────

function fileTargets() {
  // 清单项可以是 `'rs'`，也可以是借图的 `['sqlite3', 'sqlite']` —— 后者要拿**样本**去取图
  const keyOf = (e) => (Array.isArray(e) ? e[0] : e);
  const sampleOf = (e) => (Array.isArray(e) ? e[1] : e);
  const seen = new Set();
  const out = [];
  const add = (label, svg) => {
    if (seen.has(svg)) return;
    seen.add(svg);
    out.push({ label, original: svg });
  };
  for (const e of EXACT_NAMES) add(keyOf(e), fetchIcon(sampleOf(e), 'file'));
  for (const [p, sample] of NAME_PREFIXES) add(`${p}*`, fetchIcon(sample, 'file'));
  for (const e of EXTENSIONS) add(`.${keyOf(e)}`, fetchIcon(`a.${sampleOf(e)}`, 'file'));
  for (const d of FOLDER_NAMES) {
    add(`${d}/`, fetchIcon(d, 'folder'));
    add(`${d}/ 开`, fetchIcon(d, 'folder-open'));
  }
  return out;
}

function techTargets() {
  return CATALOG.map(([kind, , devicon, label]) => ({
    label,
    kind,
    original: loadIcon(devicon).svg,
  }));
}

// ─────────────────────────── 主流程 ───────────────────────────

function run(which, showAll, topN) {
  const targets = which === 'file' ? fileTargets() : techTargets();
  const rows = [];
  for (const t of targets) {
    const art = flatten(t.original, t.label);
    // 技术栈那批要复现生成器的「深色底不可读就改色」，否则比对的不是同一个东西
    const recolored = which === 'tech' && tooDarkForDarkUi(art.shapes) ? makeReadable(art.shapes) : false;
    const rebuilt = rebuildSvg(art);
    let score;
    try {
      score = diff(render(t.original), render(rebuilt));
    } catch (e) {
      score = 1;
      art.warnings.push(`渲染失败：${String(e).slice(0, 60)}`);
    }
    rows.push({
      label: t.label,
      score,
      original: t.original,
      rebuilt,
      recolored,
      warnings: [...new Set(art.warnings)],
    });
  }

  rows.sort((a, b) => b.score - a.score);
  // 改色的那批与原图比必然差得远，不该混进「需人工看」的计数里
  const judged = rows.filter((r) => !r.recolored);
  const buckets = { '>0.06 需人工看': 0, '0.02~0.06': 0, '<0.02': 0 };
  for (const r of judged) {
    if (r.score > 0.06) buckets['>0.06 需人工看']++;
    else if (r.score > 0.02) buckets['0.02~0.06']++;
    else buckets['<0.02']++;
  }
  console.log(`\n【${which}】比对 ${rows.length} 枚（其中 ${rows.length - judged.length} 枚改过色，只看形状）`);
  for (const [k, v] of Object.entries(buckets)) console.log(`  ${k.padEnd(16)} ${v}`);
  console.log('  差异榜：');
  for (const r of rows.slice(0, Math.max(topN, 20))) {
    if (r.score < 0.005 && !r.warnings.length) continue;
    const tag = r.recolored ? '[改色] ' : '';
    console.log(`    ${r.score.toFixed(4)}  ${tag}${r.label.padEnd(16)} ${r.warnings.join('；')}`);
  }

  // 拼一张 PNG：每列一枚图标，上排官方原图、下排展平重建
  const picked = showAll ? rows : rows.slice(0, topN);
  const cols = Math.min(16, picked.length);
  const cell = 56;
  const width = cols * cell;
  const height = Math.ceil(picked.length / cols) * cell * 2;
  // 内联子 SVG 定位：原图自带的 width/height 要先剥掉，否则属性重复、resvg 直接拒收
  const cellSvg = (svg, x, y) => {
    const m = svg.match(/^<svg([^>]*)>/);
    if (!m) return '';
    const attrs = m[1].replace(/\s(width|height)\s*=\s*"[^"]*"/g, '');
    return `<svg${attrs} x="${x + 4}" y="${y + 4}" width="${cell - 8}" height="${cell - 8}">${svg.slice(m[0].length)}`;
  };
  const cells = picked.map((r, i) => {
    const x = (i % cols) * cell;
    const y = Math.floor(i / cols) * cell * 2;
    return cellSvg(r.original, x, y) + cellSvg(r.rebuilt, x, y + cell);
  }).join('');
  const sheet = `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}">` +
    `<rect width="${width}" height="${height}" fill="#1c1a18"/>${cells}</svg>`;
  const png = new Resvg(sheet, { fitTo: { mode: 'width', value: width * 2 } }).render().asPng();
  fs.mkdirSync(targetDir, { recursive: true });
  const out = path.join(targetDir, `${which}-icons-diff.png`);
  fs.writeFileSync(out, png);
  console.log(`  ✓ ${path.relative(process.cwd(), out)}（${picked.length} 对，上=官方原图 下=展平重建）`);
}

function main() {
  const args = process.argv.slice(2);
  const showAll = args.includes('--all');
  const topN = Number(args.find((a) => /^\d+$/.test(a))) || 32;
  const which = args.filter((a) => a === 'file' || a === 'tech');
  for (const w of which.length ? which : ['file', 'tech']) run(w, showAll, topN);
}

main();
