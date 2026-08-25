#!/usr/bin/env node
/**
 * 图标烘焙管线：官方 SVG → 「若干条带各自颜色的笔」→ Rust 的 `Shape` 表。
 *
 * 两个生成器共用这一份：`gen_file_icons.mjs`（Material Icon Theme 的文件/目录图标）
 * 与 `gen_tech_icons.mjs`（devicon 的技术栈 logo）。它们只是取图的来源与索引表不同，
 * 「把一张任意 SVG 压平成 GPUI 画得出来的东西」这段完全一样。
 *
 * ## 为什么需要这么一层
 *
 * GPUI 侧不能让 gpui 去读 SVG（判据见 `icons/vector.rs` 模块注释），只能自绘；
 * 而 `icons/svg_path.rs` 只吃**一条** `d` + 一个正方形 viewBox + 一个填充规则。
 * 官方图标却什么都有：嵌套 `<g transform>`、`<style>` 类、`circle`/`rect`、
 * 渐变、`clip-path`、`display:none`。这里负责把那些统统化简掉。
 *
 * ## 管线
 *
 *   1. svgo 预处理（[`preprocess`]）：内联 `<style>` 类、形状转 path、能烘焙的 transform 就地烘焙；
 *   2. 自己走一遍 XML（[`flatten`]）：累积**剩余的** transform 矩阵（svgo 对嵌套 `<g>` 会放弃）、
 *      继承 fill / fill-rule / opacity / stroke，用 svgpath 把矩阵烘焙进 `d`
 *      （svgpath 会正确重算弧参数，这是不能自己乘一乘坐标了事的原因）；
 *   3. [`emitShapes`] 把结果写成 Rust 的 `&[Shape]` 字面量。
 *
 * ## 表达不了的东西（[`flatten`] 会逐条记进 `warnings`）
 *
 * - **渐变**：`window.paint_path` 一次只吃一个纯色，取各 stop 的均值近似
 *   （与 `brand.rs` 的 Gemini/Qwen 同一处理）；
 * - **mask / `<use>`**：整条丢弃；
 * - **clip-path**：先按包围盒判断它有没有真裁到东西，没有就直接放行，
 *   真裁了就做多边形求交（结果是折线，Rust 侧本来也要离散成折线）。
 *
 * `MT_ICON_DEBUG=1` 可以打开逐条诊断，看某一笔究竟为什么被丢。
 */
import { optimize } from 'svgo';
import svgpath from 'svgpath';
import polygonClipping from 'polygon-clipping';

/**
 * 坐标保留几位小数。
 *
 * 图标按 14px 画、viewBox 最小的一档是 24，所以 2 位小数 = 0.01/24 × 14px ≈ 0.006px，
 * 已经在亚像素以下；再多的位数只是让生成的 Rust 源码变大。
 */
const PRECISION = 2;
// ─────────────────────────── SVG 预处理 ───────────────────────────

const SVGO_CONFIG = {
  multipass: true,
  floatPrecision: PRECISION,
  plugins: [
    {
      name: 'preset-default',
      params: {
        overrides: {
          // 图标的 `<style>.st4{fill:#..}</style>` 会被多个 path 引用，
          // 默认的 onlyMatchedOnce 会放着不管，后面就没人认得 class 了
          inlineStyles: { onlyMatchedOnce: false },
          convertPathData: { applyTransforms: true, floatPrecision: PRECISION },
          // circle/ellipse 要连弧一起转，否则留下 <circle> 没人认
          convertShapeToPath: { convertArcs: true },
          // 颜色一律留成 hex：svgo 默认会把 `#000080` 压成 CSS 颜色名 `navy`，
          // 而下游只认得少数几个名字，认不出的整条笔会被当成「没有 fill」丢掉
          // （Lua 的深蓝主体就这么整块消失过）。names2hex 顺手把原本就写名字的也转掉
          convertColors: { names2hex: true, shortname: false },
          // 合并会跨颜色，我们是逐 path 取 fill 的，不能合
          mergePaths: false,
          // id 还要被 clipPath / gradient 引用，清了就断链
          cleanupIds: false,
          removeUselessDefs: false,
        },
      },
    },
    'convertStyleToAttrs',
  ],
};

/** svgo 预处理。取图那步各生成器自己做，拿到 SVG 文本后都要先过这里。 */
function preprocess(svgText) {
  return optimize(svgText, SVGO_CONFIG).data;
}

// ─────────────────────────── XML 遍历 ───────────────────────────

const TAG_RE =
  /<!--[\s\S]*?-->|<\?[\s\S]*?\?>|<!\[CDATA\[[\s\S]*?\]\]>|<\/([\w:.-]+)\s*>|<([\w:.-]+)((?:\s+[\w:.-]+\s*=\s*(?:"[^"]*"|'[^']*'))*)\s*(\/?)>/g;
const ATTR_RE = /([\w:.-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')/g;

function parseAttrs(raw) {
  const out = {};
  if (!raw) return out;
  for (const m of raw.matchAll(ATTR_RE)) out[m[1]] = m[2] ?? m[3] ?? '';
  return out;
}

/** 只遍历标签，忽略文本节点（图标里的文本只有 `<title>`，本就不画）。 */
function walk(xml, onOpen, onClose) {
  for (const m of xml.matchAll(TAG_RE)) {
    if (m[1]) onClose(m[1]);
    else if (m[2]) {
      onOpen(m[2], parseAttrs(m[3]));
      if (m[4]) onClose(m[2]);
    }
  }
}

// ─────────────────────────── 变换矩阵 ───────────────────────────

const IDENTITY = [1, 0, 0, 1, 0, 0];

function mul(m, n) {
  return [
    m[0] * n[0] + m[2] * n[1],
    m[1] * n[0] + m[3] * n[1],
    m[0] * n[2] + m[2] * n[3],
    m[1] * n[2] + m[3] * n[3],
    m[0] * n[4] + m[2] * n[5] + m[4],
    m[1] * n[4] + m[3] * n[5] + m[5],
  ];
}

const isIdentity = (m) => m.every((v, i) => Math.abs(v - IDENTITY[i]) < 1e-9);

/** `translate(..) scale(..) rotate(..) matrix(..) skewX/Y(..)` 串联成一个矩阵。 */
function parseTransform(str) {
  let out = IDENTITY;
  if (!str) return out;
  for (const m of str.matchAll(/([a-zA-Z]+)\s*\(([^)]*)\)/g)) {
    const a = m[2].split(/[\s,]+/).filter(Boolean).map(Number);
    if (a.some(Number.isNaN)) continue;
    const rad = (d) => (d * Math.PI) / 180;
    switch (m[1]) {
      case 'translate':
        out = mul(out, [1, 0, 0, 1, a[0] ?? 0, a[1] ?? 0]);
        break;
      case 'scale':
        out = mul(out, [a[0] ?? 1, 0, 0, a[1] ?? a[0] ?? 1, 0, 0]);
        break;
      case 'rotate': {
        const [ang, cx = 0, cy = 0] = a;
        const [c, s] = [Math.cos(rad(ang)), Math.sin(rad(ang))];
        out = mul(out, [1, 0, 0, 1, cx, cy]);
        out = mul(out, [c, s, -s, c, 0, 0]);
        out = mul(out, [1, 0, 0, 1, -cx, -cy]);
        break;
      }
      case 'matrix':
        if (a.length === 6) out = mul(out, a);
        break;
      case 'skewX':
        out = mul(out, [1, 0, Math.tan(rad(a[0] ?? 0)), 1, 0, 0]);
        break;
      case 'skewY':
        out = mul(out, [1, Math.tan(rad(a[0] ?? 0)), 0, 1, 0, 0]);
        break;
      default:
        break;
    }
  }
  return out;
}

// ─────────────────────────── 包围盒 ───────────────────────────

/**
 * `d` 在给定矩阵下的包围盒。曲线按「端点 + 控制点」估，结果**偏大** ——
 * 只用来回答「这个 clip 有没有真的裁掉东西」，偏大意味着更容易判成「没裁」，
 * 所以下面的判定还额外留了容差，宁可丢一笔也不要糊出框外。
 */
function bboxOf(d, matrix) {
  const box = [Infinity, Infinity, -Infinity, -Infinity];
  const add = (x, y) => {
    box[0] = Math.min(box[0], x);
    box[1] = Math.min(box[1], y);
    box[2] = Math.max(box[2], x);
    box[3] = Math.max(box[3], y);
  };
  try {
    svgpath(d).matrix(matrix).unarc().unshort().abs().iterate((seg, _i, x, y) => {
      const cmd = seg[0];
      if (cmd === 'Z') return;
      if (cmd === 'H') add(seg[1], y);
      else if (cmd === 'V') add(x, seg[1]);
      else for (let i = 1; i + 1 < seg.length; i += 2) add(seg[i], seg[i + 1]);
    });
  } catch {
    return null;
  }
  return Number.isFinite(box[0]) ? box : null;
}

/** a 是否（在容差内）整个盖住 b。 */
function covers(a, b, tol) {
  return a[0] <= b[0] + tol && a[1] <= b[1] + tol && a[2] >= b[2] - tol && a[3] >= b[3] - tol;
}

// ─────────────────────────── 多边形裁剪 ───────────────────────────

/** 曲线细分段数。与 Rust 侧 `svg_path::CURVE_SEGMENTS` 对齐 —— 反正最终都要离散。 */
const CURVE_SEGMENTS = 12;

/** `MT_ICON_DEBUG=1` 打开管线内部的逐条诊断（哪一笔为什么被丢）。 */
const debug = (msg) => {
  if (process.env.MT_ICON_DEBUG) console.error(`  · ${msg}`);
};

/** `d` → 若干闭合环（点列），曲线按段细分。用于做 clip 求交。 */
function toRings(d, matrix) {
  const rings = [];
  let ring = null;
  const bez = (p0, ps) => {
    // 三次/二次统一按三次算（二次先升阶）
    for (let i = 1; i <= CURVE_SEGMENTS; i++) {
      const t = i / CURVE_SEGMENTS;
      const u = 1 - t;
      ring.push([
        u * u * u * p0[0] + 3 * u * u * t * ps[0][0] + 3 * u * t * t * ps[1][0] + t * t * t * ps[2][0],
        u * u * u * p0[1] + 3 * u * u * t * ps[0][1] + 3 * u * t * t * ps[1][1] + t * t * t * ps[2][1],
      ]);
    }
  };
  try {
    svgpath(d).matrix(matrix).unarc().unshort().abs().iterate((seg, _i, x, y) => {
      const cmd = seg[0];
      if (cmd === 'M') {
        if (ring && ring.length > 2) rings.push(ring);
        ring = [[seg[1], seg[2]]];
      } else if (!ring) {
        return;
      } else if (cmd === 'L') ring.push([seg[1], seg[2]]);
      else if (cmd === 'H') ring.push([seg[1], y]);
      else if (cmd === 'V') ring.push([x, seg[1]]);
      else if (cmd === 'C') bez([x, y], [[seg[1], seg[2]], [seg[3], seg[4]], [seg[5], seg[6]]]);
      else if (cmd === 'Q') {
        const c = [seg[1], seg[2]];
        const to = [seg[3], seg[4]];
        bez([x, y], [
          [x + (2 / 3) * (c[0] - x), y + (2 / 3) * (c[1] - y)],
          [to[0] + (2 / 3) * (c[0] - to[0]), to[1] + (2 / 3) * (c[1] - to[1])],
          to,
        ]);
      } else if (cmd === 'Z' && ring.length > 2) {
        rings.push(ring);
        ring = null;
      }
    });
  } catch {
    return null;
  }
  if (ring && ring.length > 2) rings.push(ring);
  return rings.length ? rings : null;
}

/**
 * 一条 path 的若干个环 → 一个多边形几何。
 *
 * 按 **even-odd** 语义（环两两 XOR）而不是 union：一个「外环 + 内环」的描边圈，
 * union 会把中间的洞填实。nonzero 的图形在内外环反向时结果与 even-odd 一致，
 * 而同向重叠环在图标里近乎不存在，所以这里统一走 XOR。
 */
function combineRings(rings) {
  if (rings.length === 1) return [[rings[0]]];
  const [first, ...rest] = rings.map((r) => [[r]]);
  return polygonClipping.xor(first, ...rest);
}

/** 环 → `d`。裁剪结果是折线，本来也要在 Rust 侧离散成折线，没有信息损失。 */
function ringsToPath(polys, precision) {
  const n = (v) => Number(v.toFixed(precision));
  return polys
    .flatMap((poly) => poly)
    .filter((ring) => ring.length > 2)
    .map((ring) => {
      // polygon-clipping 的环首尾点重合，去掉尾点，闭合交给 `Z`
      const pts = ring[0][0] === ring[ring.length - 1][0] && ring[0][1] === ring[ring.length - 1][1]
        ? ring.slice(0, -1)
        : ring;
      return `M${n(pts[0][0])} ${n(pts[0][1])}` + pts.slice(1).map((p) => `L${n(p[0])} ${n(p[1])}`).join('') + 'Z';
    })
    .join('');
}

// ─────────────────────────── 颜色 ───────────────────────────

const NAMED_COLORS = {
  black: [0, 0, 0], white: [255, 255, 255], red: [255, 0, 0], green: [0, 128, 0],
  blue: [0, 0, 255], yellow: [255, 255, 0], gray: [128, 128, 128], grey: [128, 128, 128],
  silver: [192, 192, 192], orange: [255, 165, 0], purple: [128, 0, 128],
};

/**
 * 渐变降级成一个纯色（`window.paint_path` 一次只吃一个纯色）。
 *
 * 按 **offset 做梯形积分**，而不是把各 stop 一平均：`offset` 决定每段颜色铺多宽，
 * 无视它的话，一条「0 到 .9 都是红、最后 .1 转紫」的渐变会被算成不红不紫的洋红
 * （Angular 就吃过这个亏）。相邻两 stop 之间按线性插值，正好是梯形面积。
 */
function averageGradient(stops) {
  const sorted = [...stops].sort((a, b) => a.offset - b.offset);
  if (sorted.length === 1) return sorted[0].color;
  let span = 0;
  const acc = [0, 0, 0];
  for (let i = 0; i + 1 < sorted.length; i++) {
    const w = sorted[i + 1].offset - sorted[i].offset;
    if (w <= 0) continue;
    span += w;
    for (let c = 0; c < 3; c++) {
      acc[c] += ((sorted[i].color[c] + sorted[i + 1].color[c]) / 2) * w;
    }
  }
  // 所有 stop 挤在同一个 offset（见过），退回等权平均
  if (span <= 0) {
    return [0, 1, 2].map((c) => Math.round(sorted.reduce((s, x) => s + x.color[c], 0) / sorted.length));
  }
  return acc.map((v) => Math.round(v / span));
}

function parseColor(raw, gradients) {
  if (!raw) return null;
  // id **大小写敏感**（`url(#linearGradient4183)`），所以 url 要在原文上匹配，
  // 不能等到小写化之后 —— 那样查渐变表必然落空，整枚图标一笔不剩
  const url = raw.trim().match(/^url\(#([^)]+)\)$/);
  const v = raw.trim().toLowerCase();
  if (v === 'none' || v === 'transparent') return null;
  if (v === 'currentcolor') return 'current';
  if (url) {
    const stops = gradients.get(url[1]);
    if (!stops || !stops.length) return null;
    return averageGradient(stops);
  }
  if (v.startsWith('#')) {
    const hex = v.slice(1);
    if (hex.length === 3 || hex.length === 4) {
      return [0, 1, 2].map((i) => parseInt(hex[i] + hex[i], 16));
    }
    if (hex.length === 6 || hex.length === 8) {
      return [0, 2, 4].map((i) => parseInt(hex.slice(i, i + 2), 16));
    }
    return null;
  }
  const rgb = v.match(/^rgba?\(([^)]+)\)$/);
  if (rgb) {
    const parts = rgb[1].split(/[\s,/]+/).filter(Boolean).map(Number);
    if (parts.length >= 3 && parts.slice(0, 3).every((n) => !Number.isNaN(n))) {
      return parts.slice(0, 3).map((n) => Math.round(n));
    }
    return null;
  }
  return NAMED_COLORS[v] ?? null;
}

// ─────────────────────────── 展平 ───────────────────────────

/** 基本形 → path 的 `d`（svgo 漏网时的兜底，正常情况下用不上）。 */
function shapeToPath(tag, a) {
  const n = (k, dflt = 0) => {
    const v = parseFloat(a[k]);
    return Number.isNaN(v) ? dflt : v;
  };
  switch (tag) {
    case 'rect': {
      const [x, y, w, h] = [n('x'), n('y'), n('width'), n('height')];
      return w > 0 && h > 0 ? `M${x} ${y}H${x + w}V${y + h}H${x}Z` : null;
    }
    case 'circle': {
      const [cx, cy, r] = [n('cx'), n('cy'), n('r')];
      return r > 0 ? `M${cx - r} ${cy}a${r} ${r} 0 1 0 ${r * 2} 0a${r} ${r} 0 1 0 ${-r * 2} 0Z` : null;
    }
    case 'ellipse': {
      const [cx, cy, rx, ry] = [n('cx'), n('cy'), n('rx'), n('ry')];
      return rx > 0 && ry > 0
        ? `M${cx - rx} ${cy}a${rx} ${ry} 0 1 0 ${rx * 2} 0a${rx} ${ry} 0 1 0 ${-rx * 2} 0Z`
        : null;
    }
    case 'line':
      return `M${n('x1')} ${n('y1')}L${n('x2')} ${n('y2')}`;
    case 'polygon':
    case 'polyline': {
      const pts = (a.points ?? '').trim().split(/[\s,]+/).filter(Boolean);
      if (pts.length < 4) return null;
      const d = `M${pts[0]} ${pts[1]}` + pts.slice(2).reduce((s, v, i) => (i % 2 ? `${s} ${v}` : `${s}L${v}`), '');
      return tag === 'polygon' ? `${d}Z` : d;
    }
    default:
      return null;
  }
}

/** 画得出来的元素（`<g>` 只贡献继承属性，不自己成笔）。 */
const DRAWABLE = new Set(['path', 'rect', 'circle', 'ellipse', 'line', 'polygon', 'polyline']);
/** 整棵子树都不画的容器。 */
const SKIPPED = new Set(['defs', 'clippath', 'mask', 'title', 'desc', 'style', 'metadata', 'filter', 'pattern', 'marker', 'symbol']);

/**
 * SVG → `{ view, shapes, warnings }`。
 *
 * `view` 是归一用的 `(min_x, min_y, 边长)`：非正方形 viewBox 按长边扩成正方形
 * 并居中（等价于 SVG 默认的 `preserveAspectRatio="xMidYMid meet"`）。
 */
function flatten(svgText, label) {
  const warnings = [];
  const vb = svgText.match(/viewBox\s*=\s*"([^"]+)"/);
  let view;
  if (vb) {
    const [minX, minY, w, h] = vb[1].trim().split(/[\s,]+/).map(Number);
    const side = Math.max(w, h);
    view = [minX - (side - w) / 2, minY - (side - h) / 2, side];
  } else {
    warnings.push('没有 viewBox，按 0 0 24 24 处理');
    view = [0, 0, 24];
  }

  // 先收一遍带 id 的几何：clipPath 里可以写 `<use xlink:href="#x">` 去引用 `<defs>`
  // 里的形状（devicon 的 Go 就是这么写的），不解开引用那个 clip 就是空的，
  // 结果是「clip 什么都不留」，整枚图标一笔不剩
  const byId = new Map();
  walk(svgText, (tag, a) => {
    if (!a.id) return;
    const t = tag.toLowerCase();
    const d = t === 'path' ? a.d : shapeToPath(t, a);
    if (d) byId.set(a.id, { d, matrix: a.transform ? parseTransform(a.transform) : IDENTITY });
  }, () => {});

  // 再收渐变的 stop 色与 clipPath 的几何（`<defs>` 可能出现在引用它的 path 之后）
  const gradients = new Map();
  /** clipPath id → `[{ d, matrix }]`，matrix 是 clipPath 内部累积的那一段。 */
  const clipPaths = new Map();
  {
    let gradId = null;
    let clipId = null;
    const clipStack = [IDENTITY];
    walk(svgText, (tag, a) => {
      const t = tag.toLowerCase();
      if (t === 'lineargradient' || t === 'radialgradient') {
        gradId = a.id ?? null;
        if (gradId) gradients.set(gradId, []);
      } else if (t === 'stop' && gradId) {
        const c = parseColor(a['stop-color'] ?? a.style?.match(/stop-color:\s*([^;]+)/)?.[1], gradients);
        if (Array.isArray(c)) {
          // offset 可写成 `.5` 或 `50%`；缺省按 0 算（SVG 规范）
          const raw = a.offset ?? '0';
          const offset = raw.trim().endsWith('%')
            ? parseFloat(raw) / 100
            : parseFloat(raw) || 0;
          gradients.get(gradId).push({ color: c, offset: Number.isFinite(offset) ? offset : 0 });
        }
      } else if (t === 'clippath') {
        clipId = a.id ?? null;
        if (clipId) clipPaths.set(clipId, []);
        clipStack.push(a.transform ? parseTransform(a.transform) : IDENTITY);
      } else if (clipId) {
        const parent = clipStack[clipStack.length - 1];
        const matrix = a.transform ? mul(parent, parseTransform(a.transform)) : parent;
        clipStack.push(matrix);
        if (t === 'use') {
          const href = (a['xlink:href'] ?? a.href ?? '').replace(/^#/, '');
          const ref = byId.get(href);
          if (ref) clipPaths.get(clipId).push({ d: ref.d, matrix: mul(matrix, ref.matrix) });
          else debug(`${label}: clipPath ${clipId} 引用了找不到的 #${href}`);
          return;
        }
        const d = t === 'path' ? a.d : shapeToPath(t, a);
        if (d) clipPaths.get(clipId).push({ d, matrix });
      }
    }, (tag) => {
      const t = tag.toLowerCase();
      if (t === 'lineargradient' || t === 'radialgradient') gradId = null;
      else if (t === 'clippath') {
        clipId = null;
        clipStack.length = 1;
      } else if (clipId && clipStack.length > 1) clipStack.pop();
    });
  }

  /**
   * 这个 clip 有没有真的裁掉东西。
   *
   * Material 的图标里 clip 有两种：一种是**覆盖全图的边界框**（Jenkins 那枚，
   * clipPath 是块 145×145 的矩形而 viewBox 就 180），忽略它毫无影响，直接放行；
   * 另一种是真裁（node_modules 拿六边形去裁一个大三角），走下面的求交。
   */
  const clipIsNoop = (clip, d, matrix) => {
    const parts = clipPaths.get(clip.id);
    if (!parts || !parts.length) return false;
    const target = bboxOf(d, matrix);
    if (!target) return false;
    let union = null;
    for (const p of parts) {
      const b = bboxOf(p.d, mul(clip.base, p.matrix));
      if (!b) return false;
      union = union
        ? [Math.min(union[0], b[0]), Math.min(union[1], b[1]), Math.max(union[2], b[2]), Math.max(union[3], b[3])]
        : b;
    }
    return union ? covers(union, target, view[2] * 0.01) : false;
  };

  /**
   * 真裁剪：把被裁形状与 clip 形状求交，交集就是该画的东西。
   *
   * 结果是折线 —— Rust 侧本来就把曲线离散成折线画，没有信息损失，只是 `d` 变长，
   * 而走到这一步的图标全仓不过两三枚。求不出来（自交、退化）就返回 `null`，
   * 由调用方丢弃并记警告。
   */
  const clipIntersect = (clip, d, matrix) => {
    const parts = clipPaths.get(clip.id);
    if (!parts || !parts.length) return null;
    const subject = toRings(d, matrix);
    if (!subject) return null;
    const clipRings = [];
    for (const p of parts) {
      const r = toRings(p.d, mul(clip.base, p.matrix));
      if (!r) return null;
      clipRings.push(...r);
    }
    try {
      // 多环先按 even-odd 语义 XOR 成一个几何 —— 直接把每个环当独立多边形是 union，
      // 会把「外环 + 内环」挖出来的洞填实（Jenkins 的黑色描边环就这么变成整片剪影的）
      const result = polygonClipping.intersection(combineRings(subject), combineRings(clipRings));
      if (!result || !result.length) {
        const bb = (rings) => {
          const xs = rings.flat().map((p) => p[0]);
          const ys = rings.flat().map((p) => p[1]);
          return `[${Math.min(...xs).toFixed(1)},${Math.min(...ys).toFixed(1)} → ${Math.max(...xs).toFixed(1)},${Math.max(...ys).toFixed(1)}]`;
        };
        debug(`${label}: clip 求交为空 subject${bb(subject)} clip${bb(clipRings)}`);
        return null;
      }
      const out = ringsToPath(result, PRECISION);
      return out || null;
    } catch (e) {
      debug(`${label}: clip 求交抛异常 ${String(e).slice(0, 120)}`);
      return null;
    }
  };

  const shapes = [];
  // 继承栈：每层记住 transform / fill / stroke / opacity 等
  const stack = [{
    matrix: IDENTITY, fill: null, fillRule: null, fillOpacity: 1,
    stroke: null, strokeWidth: 1, strokeOpacity: 1, opacity: 1, clip: null, masked: false,
  }];
  let skipDepth = 0;

  walk(svgText, (tag, a) => {
    const t = tag.toLowerCase();
    if (skipDepth > 0) {
      skipDepth++;
      return;
    }
    // `display:none` 的子树整棵不渲染（子元素写 display:inline 也翻不了案，SVG 1.1 §11.3）
    if (SKIPPED.has(t) || a.display === 'none') {
      skipDepth = 1;
      return;
    }
    if (t === 'use') {
      warnings.push('丢弃 <use>（引用展开没做）');
      return;
    }

    const parent = stack[stack.length - 1];
    const num = (k, dflt) => {
      const v = parseFloat(a[k]);
      return Number.isNaN(v) ? dflt : v;
    };
    const matrix = a.transform ? mul(parent.matrix, parseTransform(a.transform)) : parent.matrix;
    const clipRef = a['clip-path']?.trim().match(/^url\(#([^)]+)\)$/);
    const cur = {
      matrix,
      fill: 'fill' in a ? a.fill : parent.fill,
      fillRule: a['fill-rule'] ?? a['clip-rule'] ?? parent.fillRule,
      fillOpacity: num('fill-opacity', parent.fillOpacity),
      stroke: 'stroke' in a ? a.stroke : parent.stroke,
      strokeWidth: num('stroke-width', parent.strokeWidth),
      strokeOpacity: num('stroke-opacity', parent.strokeOpacity),
      opacity: parent.opacity * num('opacity', 1),
      // clip 记的是「引用它的那个节点的 CTM」——clipPath 的内容就在这个坐标系里
      clip: clipRef ? { id: clipRef[1], base: matrix } : parent.clip,
      masked: parent.masked || !!a.mask,
    };
    stack.push(cur);
    if (t === 'svg' || t === 'g') return;
    if (!DRAWABLE.has(t)) return;

    if (cur.masked) {
      warnings.push(`丢弃 <${t} mask>（蒙版没做）`);
      return;
    }
    let d = t === 'path' ? a.d : shapeToPath(t, a);
    if (!d) return;
    if (cur.clip && !clipIsNoop(cur.clip, d, cur.matrix)) {
      const clipped = clipIntersect(cur.clip, d, cur.matrix);
      if (!clipped) {
        warnings.push(`丢弃 <${t} clip-path>（求交失败）`);
        return;
      }
      // 求交的结果已经在根坐标系里了，矩阵不能再乘第二遍
      warnings.push('clip 求交（结果是折线）');
      d = clipped;
    } else if (!isIdentity(cur.matrix)) {
      // 弧命令在矩阵下要重算半径与倾角，不是乘一乘坐标了事 —— 交给 svgpath
      d = svgpath(d).matrix(cur.matrix).round(PRECISION).toString();
    }
    // 没有变换就原样留着 svgo 的输出：它的相对命令 + 省略前导零比
    // svgpath 重新序列化出来的绝对命令短一大截，而这是绝大多数 path 的情形
    if (!d) return;

    // fill 缺省是黑色（SVG 规范）。Material 的图标基本都显式给色，所以缺省黑
    // 十有八九是继承丢了 —— 一条大黑 path 盖上去，整枚图标就成了剪影
    const inherited = cur.fill === null || cur.fill === undefined;
    if (inherited) debug(`${label}: <${t}> 没有 fill，按黑色画（d 起头 ${String(d).slice(0, 34)}）`);
    const fillRaw = inherited ? '#000' : cur.fill;
    const fill = parseColor(fillRaw, gradients);
    if (/url\(#/.test(String(fillRaw)) && Array.isArray(fill)) {
      warnings.push('渐变降级成单色');
    }
    if (fill) {
      shapes.push({
        pen: 'fill',
        color: fill,
        alpha: cur.opacity * cur.fillOpacity,
        evenOdd: cur.fillRule === 'evenodd',
        d,
      });
    }
    const stroke = parseColor(cur.stroke, gradients);
    if (stroke && cur.strokeWidth > 0) {
      shapes.push({
        pen: 'stroke',
        color: stroke,
        alpha: cur.opacity * cur.strokeOpacity,
        // 描边宽度归一成「单位方框的比例」（Shape::line 的口径）；
        // 矩阵有缩放时线宽也跟着缩，取行列式的平方根作等效缩放
        width: (cur.strokeWidth * Math.sqrt(Math.abs(cur.matrix[0] * cur.matrix[3] - cur.matrix[1] * cur.matrix[2]))) / view[2],
        d,
      });
    }
  }, (tag) => {
    const t = tag.toLowerCase();
    if (skipDepth > 0) {
      skipDepth--;
      return;
    }
    if (SKIPPED.has(t) || t === 'use') return;
    if (stack.length > 1) stack.pop();
  });

  if (!shapes.length) warnings.push('展平后一笔都不剩');

  // 越界自检：官方图偶尔画出 viewBox 一丁点，原版靠 SVG 视口裁掉，自绘不裁。
  // 溢出零点几个百分点在 14px 上看不出来，但得让它有个说法，别哪天真的糊到邻居身上。
  //
  // 必须用**离散后的实际点**（toRings），不能用 bboxOf —— 后者把贝塞尔控制点也算进去，
  // 而控制点常常远在曲线外侧，拿它判越界会虚报十几个百分点。
  let overflow = 0;
  for (const s of shapes) {
    for (const ring of toRings(s.d, IDENTITY) ?? []) {
      for (const [x, y] of ring) {
        overflow = Math.max(
          overflow,
          (view[0] - x) / view[2], (view[1] - y) / view[2],
          (x - view[0] - view[2]) / view[2], (y - view[1] - view[2]) / view[2],
        );
      }
    }
  }
  if (overflow > 0.002) warnings.push(`几何超出画布 ${(overflow * 100).toFixed(1)}%`);

  return { view, shapes, warnings, label };
}
// ─────────────────────────── Rust 代码生成 ───────────────────────────

/** 形状表的内容指纹 —— 去重靠它（`.jpg` 与 `.jpeg` 拿到的是同一张图）。 */
const fingerprint = (art) =>
  JSON.stringify([art.view, art.shapes.map((s) => [s.pen, s.color, s.alpha, s.evenOdd, s.width, s.d])]);

const rustIdent = (name) => name.toUpperCase().replace(/[^A-Z0-9]+/g, '_').replace(/^(\d)/, 'N$1');
const f32 = (v) => {
  const s = Number(v.toFixed(4)).toString();
  return s.includes('.') || s.includes('e') ? s : `${s}.0`;
};

function emitShapes(art) {
  const view = `(${f32(art.view[0])}, ${f32(art.view[1])}, ${f32(art.view[2])})`;
  const body = art.shapes.map((s) => {
    const [r, g, b] = s.color === 'current' ? [null, null, null] : s.color;
    const hex = (v) => `0x${v.toString(16).padStart(2, '0')}`;
    const ink = s.color === 'current'
      ? (s.alpha < 0.999 ? `Ink::CurrentAlpha(${f32(s.alpha)})` : 'Ink::Current')
      : (s.alpha < 0.999
        ? `Ink::RgbAlpha(${hex(r)}, ${hex(g)}, ${hex(b)}, ${f32(s.alpha)})`
        : `Ink::Rgb(${hex(r)}, ${hex(g)}, ${hex(b)})`);
    // 描边笔没有填充规则可言,但字段得有值 —— 统一按 false 写
    const geom = `Geom::Path {\n            d: "${s.d}",\n            view: ${view},\n            even_odd: ${s.evenOdd === true},\n        }`;
    return s.pen === 'fill'
      ? `    Shape::fill(\n        ${ink},\n        ${geom},\n    ),`
      : `    Shape::line(\n        ${ink},\n        ${f32(s.width)},\n        ${geom},\n    ),`;
  }).join('\n');
  return `&[\n${body}\n]`;
}

/** 把展平结果拼回一张 SVG —— 只为在预览页 / 验收脚本里和原图对照。 */
function rebuildSvg(art) {
  const [x, y, side] = art.view;
  const body = art.shapes.map((s) => {
    const color = s.color === 'current' ? '#f0ece6' : `rgb(${s.color.join(',')})`;
    const alpha = s.alpha < 0.999 ? ` opacity="${s.alpha}"` : '';
    return s.pen === 'fill'
      ? `<path d="${s.d}" fill="${color}" fill-rule="${s.evenOdd ? 'evenodd' : 'nonzero'}"${alpha}/>`
      : `<path d="${s.d}" fill="none" stroke="${color}" stroke-width="${s.width * side}"${alpha}/>`;
  }).join('');
  return `<svg xmlns="http://www.w3.org/2000/svg" viewBox="${x} ${y} ${side} ${side}">${body}</svg>`;
}

export {
  PRECISION, SVGO_CONFIG, preprocess,
  flatten, rebuildSvg,
  fingerprint, rustIdent, f32, emitShapes,
  debug, bboxOf,
};
