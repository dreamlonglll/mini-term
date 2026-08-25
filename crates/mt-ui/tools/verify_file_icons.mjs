#!/usr/bin/env node
/**
 * 图标展平管线的验收台：**同一个渲染器**分别渲染「官方原图」与「展平重建图」，
 * 逐像素比对，把差得最多的排前面 —— 管线丢了 transform / 漏了一笔 / 颜色错了，
 * 都会在这张榜上冒头，不必肉眼扫两百多枚。
 *
 * ```bash
 * node verify_file_icons.mjs            # 打榜 + 拼一张差异最大的对照图
 * node verify_file_icons.mjs 40         # 对照图里放 40 对
 * node verify_file_icons.mjs --all      # 全部图标拼图（验收最终成果用）
 * ```
 *
 * 产物 `target/file-icons-diff.png`（上=官方原图，下=展平重建）。
 *
 * 差异度是 0..1 的平均像素距离（含 alpha）。经验阈值：
 * `< 0.02` 肉眼看不出；`0.02..0.06` 多半是抗锯齿/曲线离散的正常抖动；
 * `> 0.06` 要人工看，通常意味着真丢了东西。
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { Resvg } from '@resvg/resvg-js';
import {
  fetchIcon, flatten, rebuildSvg,
  EXACT_NAMES, NAME_PREFIXES, EXTENSIONS, FOLDER_NAMES,
} from './gen_file_icons.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const outPng = path.resolve(here, '..', '..', '..', 'target', 'file-icons-diff.png');

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

function main() {
  const arg = process.argv[2];
  const showAll = process.argv.includes('--all');
  const topN = Number(arg) || 32;

  const targets = [
    ...EXACT_NAMES.map((k) => ['file', k, k]),
    ...NAME_PREFIXES.map(([p, sample]) => ['file', sample, `${p}*`]),
    ...EXTENSIONS.map((e) => ['file', `a.${e}`, `.${e}`]),
    ...FOLDER_NAMES.flatMap((d) => [['folder', d, `${d}/`], ['folder-open', d, `${d}/ 开`]]),
  ];

  const rows = [];
  const seen = new Set();
  for (const [kind, key, label] of targets) {
    const original = fetchIcon(key, kind);
    if (seen.has(original)) continue;
    seen.add(original);
    const art = flatten(original, label);
    const rebuilt = rebuildSvg(art);
    let score;
    try {
      score = diff(render(original), render(rebuilt));
    } catch (e) {
      score = 1;
      art.warnings.push(`渲染失败：${String(e).slice(0, 60)}`);
    }
    rows.push({ label, score, original, rebuilt, warnings: [...new Set(art.warnings)] });
  }

  rows.sort((a, b) => b.score - a.score);
  const buckets = { '>0.06 需人工看': 0, '0.02~0.06': 0, '<0.02': 0 };
  for (const r of rows) {
    if (r.score > 0.06) buckets['>0.06 需人工看']++;
    else if (r.score > 0.02) buckets['0.02~0.06']++;
    else buckets['<0.02']++;
  }
  console.log(`比对 ${rows.length} 枚（去重后）`);
  for (const [k, v] of Object.entries(buckets)) console.log(`  ${k.padEnd(16)} ${v}`);
  console.log('\n差异榜：');
  for (const r of rows.slice(0, Math.max(topN, 20))) {
    if (r.score < 0.005 && !r.warnings.length) continue;
    console.log(`  ${r.score.toFixed(4)}  ${r.label.padEnd(18)} ${r.warnings.join('；')}`);
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
  fs.mkdirSync(path.dirname(outPng), { recursive: true });
  fs.writeFileSync(outPng, png);
  console.log(`\n✓ ${path.relative(process.cwd(), outPng)}（${picked.length} 对，上=官方原图 下=展平重建）`);
}

main();
