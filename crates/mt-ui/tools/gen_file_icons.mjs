#!/usr/bin/env node
/**
 * 文件树图标生成器：@baybreezy/file-extension-icon（Material Icon Theme）
 *   → crates/mt-ui/src/icons/file_art.rs
 *
 * 原版（Tauri/React）用这个 npm 包按文件名取一枚**专属图标**，一类型一张图；
 * GPUI 侧没有 asset source 也不能走位图 SVG（判据见 `icons/vector.rs` 模块注释），
 * 但 `icons/svg_path.rs` 能原样解析官方那条 `d` —— 于是把官方几何 + 官方色
 * 烘焙成 Rust 静态形状表，渲染仍是自绘。厂商 logo（`brand.rs`）走的是同一条路，
 * 只是那边十一枚手抄得过来，文件图标两百枚必须脚本生成。
 *
 * ## 用法
 *
 * ```bash
 * cd crates/mt-ui/tools && npm install     # 装 3 个生成期依赖（node_modules 已 gitignore）
 * node gen_file_icons.mjs                  # 覆写 ../src/icons/file_art.rs
 * node gen_file_icons.mjs --preview        # 另出 target/file-icons-preview.html 供肉眼验收
 * ```
 *
 * 产物 `file_art.rs` 是生成物，**禁止手改**；要改覆盖面就改本文件的 KEY 清单后重跑。
 *
 * ## 管线
 *
 *   1. `getMaterialFileIcon(name)` / `getMaterialFolderIcon(name, open)` 拿到 data URI，解出 SVG；
 *   2. svgo 预处理：内联 `<style>` 类、`circle`/`rect` 等形状转 path、能烘焙的 transform 就地烘焙；
 *   3. 自己走一遍 XML：累积**剩余的** transform 矩阵（svgo 对嵌套 `<g>` 会放弃）、
 *      继承 fill / fill-rule / opacity / stroke，用 svgpath 把矩阵烘焙进 `d`
 *      （svgpath 会正确重算弧参数，这是不能自己乘坐标了事的原因）；
 *   4. 按「展平结果」去重 —— 200+ 个 key 里大量指向同一枚图（`.jpg`/`.jpeg` 同图），
 *      去重后才是真正要写进 Rust 的图标数；
 *   5. 生成形状表 + 四张索引表（整名 / 前缀 / 扩展名 / 目录名），索引表按 key 排序供二分查找。
 *
 * ## 表达不了的东西（会在运行日志里逐条列出）
 *
 * - **渐变**：`window.paint_path` 一次只吃一个纯色，取各 stop 的均值近似（与 brand.rs 的
 *   Gemini/Qwen 同一处理）；
 * - **clip-path / mask / `<use>`**：整条丢弃。保留它等于把「本该被裁掉的那部分」画出来，
 *   比缺一笔难看得多。受影响的图标在日志里点名，验收时逐枚看过。
 */
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { optimize } from 'svgo';
import svgpath from 'svgpath';
import polygonClipping from 'polygon-clipping';
import { getMaterialFileIcon, getMaterialFolderIcon } from '@baybreezy/file-extension-icon';

const here = path.dirname(fileURLToPath(import.meta.url));
const outFile = path.resolve(here, '..', 'src', 'icons', 'file_art.rs');
const previewFile = path.resolve(here, '..', '..', '..', 'target', 'file-icons-preview.html');

/**
 * 坐标保留几位小数。
 *
 * 图标按 14px 画、viewBox 最小的一档是 24，所以 2 位小数 = 0.01/24 × 14px ≈ 0.006px，
 * 已经在亚像素以下；再多的位数只是让生成的 Rust 源码变大。
 */
const PRECISION = 2;

// ─────────────────────────── KEY 清单 ───────────────────────────
// 也被 verify_file_icons.mjs 复用（同一份清单，验收的就是要生成的那批）。
export { EXACT_NAMES, NAME_PREFIXES, EXTENSIONS, FOLDER_NAMES };

// 覆盖面沿用改造前 file.rs 那三张手工表（它是照着原版整理的），
// 顺序语义也照旧：**整名 → 前缀 → 扩展名 → 兜底**。
// `Cargo.lock` 是锁文件不是 toml、`Dockerfile` 没有扩展名 —— 这条是
// Material Icon Theme 的核心语义，顺序错了整张表就废了。

/**
 * 整名命中（全小写比对）。必须在扩展名之前查。
 *
 * 与 [`EXTENSIONS`] 一样支持 `['rakefile', 'makefile']` 的借图写法。
 */
const EXACT_NAMES = [
  // 锁文件
  'package-lock.json', 'yarn.lock', 'pnpm-lock.yaml', ['bun.lockb', 'cargo.lock'],
  'cargo.lock', 'poetry.lock', 'composer.lock', 'gemfile.lock', 'go.sum', 'uv.lock',
  // 构建 / 包清单
  'cargo.toml', 'go.mod', 'package.json', 'composer.json', 'gemfile',
  ['rakefile', 'makefile'],
  'pubspec.yaml', 'pyproject.toml', 'requirements.txt', 'setup.py', 'pom.xml',
  'build.gradle', 'build.gradle.kts', 'settings.gradle', 'makefile',
  ['gnumakefile', 'makefile'],
  'cmakelists.txt', ['justfile', 'makefile'], 'tsconfig.json', 'jsconfig.json', 'vite.config.ts',
  'webpack.config.js', 'rollup.config.js', 'tailwind.config.js', 'eslint.config.js',
  // git 元数据
  '.gitignore', '.gitattributes', '.gitmodules', '.gitkeep', '.mailmap',
  // 容器 / CI
  'dockerfile', ['containerfile', 'dockerfile'], '.dockerignore', 'docker-compose.yml',
  '.gitlab-ci.yml', '.travis.yml', 'jenkinsfile', 'vercel.json', 'netlify.toml',
  // 各家 rc / 配置裸文件
  '.editorconfig', '.npmrc', '.nvmrc', '.babelrc', '.prettierrc', '.eslintrc',
  '.browserslistrc', '.htaccess',
  // 文档
  'readme', 'readme.md', 'changelog.md', 'contributing.md', 'license', 'licence',
  'copying', ['notice', 'license'], 'authors', 'todo.md',
  // 本仓自己的
  'claude.md', 'agents.md',
];

/**
 * 前缀命中（整名没中时按前缀兜）。同样在扩展名之前。
 *
 * 第二项是**取图样本**：库只按整名/扩展名查表，不认识「前缀」这回事，
 * 拿 `dockerfile.dev` 去问只会得到通用文件图 —— 要用 `dockerfile` 问，
 * 再把那枚图挂到 `dockerfile.` 这条前缀上。
 */
const NAME_PREFIXES = [
  ['.env', '.env'],
  ['dockerfile.', 'dockerfile'],
  ['docker-compose', 'docker-compose.yml'],
  ['.eslintrc.', '.eslintrc'],
  ['.prettierrc.', '.prettierrc'],
  ['license.', 'license'],
];

/**
 * 扩展名 → 图标。
 *
 * 写成 `['sqlite3', 'sqlite']` 表示**借图**：库里没收录 `.sqlite3`，直接问它只会
 * 拿到通用文件图，于是拿 `.sqlite` 的图挂上去。只在「同一类东西」之间借
 * （`.wav` 借 `.mp3`、`.so` 借 `.dll`）；`.astro` 借 `.svelte` 这种跨框架的不做 ——
 * 顶着别家框架的 logo 比顶着通用图标更误导。
 */
const EXTENSIONS = [
  // 系统级语言
  'rs', 'go', 'zig', 'c', 'h', 'cc', 'cpp', 'cxx', 'hpp', 'hh', 'hxx', 'm', 'mm',
  // JVM / .NET / 移动端
  'java', 'kt', 'kts', ['scala', 'sbt'], 'sbt', 'groovy', 'cs', 'csx', 'csproj', 'sln',
  'fs', 'swift', 'dart',
  // 脚本语言
  'py', ['pyi', 'py'], ['pyw', 'py'], 'ipynb', 'rb', 'erb', ['gemspec', 'rb'],
  'php', ['phtml', 'php'], 'lua', 'pl',
  'hs', ['lhs', 'hs'], 'ex', 'exs', 'erl', ['hrl', 'erl'], 'nim', 'r', 'jl', 'sol', 'vim',
  // 前端
  'js', 'mjs', ['cjs', 'js'], 'ts', ['mts', 'ts'], ['cts', 'ts'], 'jsx', 'tsx',
  'vue', 'svelte', 'astro',
  'html', 'htm', 'xhtml', 'ejs', 'hbs', 'css', 'scss', 'sass', 'less', 'styl',
  // 标记 / 数据
  'xml', 'xsl', 'xsd', 'plist', 'json', 'json5', ['jsonc', 'json'], 'ndjson', 'jsonl',
  'yaml', 'yml', 'toml', 'ini', 'cfg', 'conf', 'properties', 'csv', 'tsv', 'env',
  'graphql', 'gql', 'proto', 'tf', 'tfvars', 'hcl', 'gradle', 'cmake', 'mk', 'ninja',
  // 文档
  'md', 'mdx', 'markdown', 'rst', 'adoc', 'txt', ['text', 'txt'], 'rtf', 'doc', 'docx',
  ['odt', 'docx'], 'xls', 'xlsx', 'ppt', 'pptx', 'pdf', 'log',
  // 媒体
  'png', 'jpg', 'jpeg', 'gif', 'bmp', 'webp', 'ico', ['icns', 'ico'], 'tif', 'tiff',
  ['avif', 'webp'], 'heic', 'psd', 'ai', 'sketch', 'fig', 'svg',
  'mp4', 'mkv', 'mov', 'avi', 'webm', 'flv', 'wmv', 'm4v',
  'mp3', ['wav', 'mp3'], 'flac', 'ogg', ['aac', 'mp3'], 'm4a', ['opus', 'mp3'], 'wma',
  // 归档 / 二进制
  'zip', 'tar', 'gz', 'tgz', ['bz2', 'gz'], 'xz', '7z', 'rar', ['zst', 'gz'], ['lz4', 'gz'],
  'jar', ['war', 'jar'], ['ear', 'jar'], 'exe', 'dll', ['so', 'dll'], ['dylib', 'dll'],
  ['bin', 'dll'], 'wasm', ['o', 'dll'], ['a', 'dll'], 'lib',
  'obj', 'pdb', ['class', 'java'], 'pyc', 'msi', ['dmg', 'exe'], 'apk', ['deb', 'exe'],
  ['rpm', 'exe'], ['appimage', 'exe'],
  // 字体 / 数据库 / 壳 / 证书
  'ttf', 'otf', 'woff', 'woff2', 'eot',
  'db', 'sqlite', ['sqlite3', 'sqlite'], 'mdb', 'accdb', ['realm', 'sqlite'], 'sql',
  ['ddl', 'sql'], 'prisma',
  'sh', 'bash', 'zsh', 'fish', 'ksh', 'bat', 'cmd', 'ps1', 'psm1', 'psd1',
  'lock', 'pem', 'key', 'crt', 'cer', ['pfx', 'pem'], ['p12', 'pem'], 'pub', 'asc', 'gpg',
  'patch', ['diff', 'patch'], 'http', 'rest',
];

/**
 * 目录名 → 专属图标（原版有、改造前的 GPUI 版没有的那条能力）。
 *
 * 清单是「候选」而非「白名单」：逐个问库要图，**只有拿到非通用图的才会进表**，
 * 落选的自动回落通用文件夹。所以这里可以放心多列。
 */
const FOLDER_NAMES = [
  'src', 'source', 'lib', 'libs', 'app', 'apps', 'core', 'common', 'shared',
  'test', 'tests', 'spec', '__tests__', 'e2e', 'benchmark', 'coverage',
  'docs', 'doc', 'documentation', 'examples', 'example', 'demo',
  'dist', 'build', 'out', 'target', 'bin', 'obj', 'release', 'debug',
  'node_modules', 'vendor', 'packages', 'crates', 'modules', 'plugins', 'extensions',
  '.git', '.github', '.gitlab', '.vscode', '.idea', '.cargo', '.claude', '.husky',
  'assets', 'images', 'image', 'img', 'icons', 'fonts', 'audio', 'video', 'media',
  'components', 'component', 'views', 'view', 'pages', 'page', 'layouts', 'layout',
  'templates', 'partials', 'widgets', 'screens', 'containers', 'elements',
  'config', 'configs', 'settings', 'environment', 'environments',
  'public', 'static', 'www', 'web', 'client', 'server', 'api', 'services', 'service',
  'utils', 'util', 'helpers', 'tools', 'scripts', 'script', 'hooks', 'middleware',
  'models', 'model', 'controllers', 'routes', 'router', 'store', 'stores', 'context',
  'types', 'typings', 'interfaces', 'constants', 'enums', 'themes', 'theme',
  'styles', 'style', 'css', 'sass', 'less',
  'i18n', 'locales', 'lang', 'translations',
  'database', 'db', 'migrations', 'seeders', 'sql',
  'android', 'ios', 'mobile', 'desktop', 'linux', 'windows', 'macos',
  'ci', 'cd', 'deploy', 'docker', 'kubernetes', 'k8s', 'terraform', 'ansible',
  'temp', 'tmp', 'cache', 'logs', 'log', 'archive', 'backup', 'downloads',
  'security', 'auth', 'keys', 'certificates', 'secrets',
  'functions', 'lambda', 'workflows', 'jobs', 'tasks', 'queue', 'events',
  'mocks', 'fixtures', 'stubs', 'seed', 'batch', 'aurelia', 'meta',
];

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
    // 渐变降级成单色：各 stop 求均值（两个 stop 时正好是中点，与 brand.rs 的 Qwen 同口径）
    const acc = stops.reduce((s, c) => [s[0] + c[0], s[1] + c[1], s[2] + c[2]], [0, 0, 0]);
    return acc.map((x) => Math.round(x / stops.length));
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
export function flatten(svgText, label) {
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

  // 先收一遍渐变的 stop 色与 clipPath 的几何（`<defs>` 可能出现在引用它的 path 之后）
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
        if (Array.isArray(c)) gradients.get(gradId).push(c);
      } else if (t === 'clippath') {
        clipId = a.id ?? null;
        if (clipId) clipPaths.set(clipId, []);
        clipStack.push(a.transform ? parseTransform(a.transform) : IDENTITY);
      } else if (clipId) {
        const parent = clipStack[clipStack.length - 1];
        const matrix = a.transform ? mul(parent, parseTransform(a.transform)) : parent;
        clipStack.push(matrix);
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
  // 溢出零点几个百分点在 14px 上看不出来，但得让它有个说法，别哪天真的糊到邻居身上
  let overflow = 0;
  for (const s of shapes) {
    const b = bboxOf(s.d, IDENTITY);
    if (!b) continue;
    overflow = Math.max(
      overflow,
      (view[0] - b[0]) / view[2], (view[1] - b[1]) / view[2],
      (b[2] - view[0] - view[2]) / view[2], (b[3] - view[1] - view[2]) / view[2],
    );
  }
  if (overflow > 0.002) warnings.push(`几何超出画布 ${(overflow * 100).toFixed(1)}%`);

  return { view, shapes, warnings, label };
}

// ─────────────────────────── 取图 ───────────────────────────

const dataUriToSvg = (uri) => Buffer.from(uri.split(',')[1], 'base64').toString('utf8');

export function fetchIcon(key, kind) {
  const raw = kind === 'file'
    ? getMaterialFileIcon(key)
    : getMaterialFolderIcon(key, kind === 'folder-open');
  return optimize(dataUriToSvg(raw), SVGO_CONFIG).data;
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

function emitRust(arts, index) {
  const lines = [];
  const push = (s = '') => lines.push(s);

  push('//! 文件树图标的形状表 —— **生成物，禁止手改**。');
  push('//!');
  push('//! 由 `crates/mt-ui/tools/gen_file_icons.mjs` 从 `@baybreezy/file-extension-icon`');
  push('//! （Material Icon Theme，MIT，与原版 Tauri 前端同一个包）烘焙而来：官方 SVG 的');
  push('//! 那条 `d` 原样搬进 [`Geom::Path`]，官方色写成 [`Ink::Rgb`]，渲染仍是自绘');
  push('//! （判据见 [`super::vector`] 模块注释）。改覆盖面请改生成器的 KEY 清单后重跑。');
  push('//!');
  push('//! 图标版权归 Material Icon Theme（Philipp Kief，MIT）与各自的商标持有者；');
  push('//! 与原版一样只作「这一行是什么类型的文件」的指示性使用。');
  push('');
  push('use super::vector::{Geom, Ink, Shape};');
  push('');
  push('/// 一枚图标：官方图标名 + 若干条带各自颜色的笔。');
  push('///');
  push('/// `name` 是 Material Icon Theme 的图标名，只用于对账与单测断言 ——');
  push('/// 它让「`Cargo.lock` 该拿到 lock 图标」这类语义能被直接测，而不必比较几何。');
  push('#[derive(Clone, Copy, Debug)]');
  push('pub struct FileArt {');
  push('    pub name: &\'static str,');
  push('    pub shapes: &\'static [Shape],');
  push('}');
  push('');

  for (const art of arts) {
    // 名字取「第一个请到这枚图的 key」；共用它的其余 key 一并列出，方便对账
    // （`.jpg` 与 `.png` 是不是同一张、`Cargo.lock` 有没有跟 `.toml` 串台，一眼可见）
    const keys = art.keys.slice(0, 12).join(' ') + (art.keys.length > 12 ? ` …共 ${art.keys.length} 个` : '');
    push(`/// \`${art.name}\` — ${keys}`);
    push(`static ${rustIdent(art.name)}: &[Shape] = ${emitShapes(art)};`);
    push('');
  }

  push('/// 全部图标。索引表里的下标指向这里。');
  push('pub static ARTS: &[FileArt] = &[');
  for (const art of arts) {
    push(`    FileArt { name: "${art.name}", shapes: ${rustIdent(art.name)} },`);
  }
  push('];');
  push('');

  const table = (name, doc, rows, cols) => {
    push(`/// ${doc}`);
    push(`pub static ${name}: &[(&str, ${cols === 2 ? 'u16' : '(u16, u16)'})] = &[`);
    for (const [key, v] of rows) {
      push(`    ("${key}", ${cols === 2 ? v : `(${v[0]}, ${v[1]})`}),`);
    }
    push('];');
    push('');
  };

  table('FILE_EXACT', '整名 → 图标下标。**必须在扩展名之前查**（`Cargo.lock` 是锁文件不是 toml）。键已排序，二分查找。', index.exact, 2);
  table('FILE_PREFIX', '前缀 → 图标下标（`.env.production` / `Dockerfile.dev`）。整名没中才查，按表内顺序线性匹配。', index.prefix, 2);
  table('FILE_EXT', '扩展名（小写、不含点）→ 图标下标。键已排序，二分查找。', index.ext, 2);
  table('FOLDER_NAMES', '目录名 → （合上, 展开）两个图标下标。键已排序，二分查找。', index.folders, 3);

  push('/// 认不出类型的文件。');
  push(`pub const FILE_FALLBACK: u16 = ${index.fallback.file};`);
  push('/// 没有专属图标的目录（合上）。');
  push(`pub const FOLDER_FALLBACK: u16 = ${index.fallback.folder};`);
  push('/// 没有专属图标的目录（展开）。');
  push(`pub const FOLDER_OPEN_FALLBACK: u16 = ${index.fallback.folderOpen};`);
  push('');

  return lines.join('\n');
}

// ─────────────────────────── 预览页 ───────────────────────────

function emitPreview(entries) {
  const cell = ({ label, original, rebuilt }) => `
  <div class="cell">
    <div class="pair"><span title="原图">${original}</span><span title="展平后">${rebuilt}</span></div>
    <code>${label.replace(/[<&]/g, (c) => (c === '<' ? '&lt;' : '&amp;'))}</code>
  </div>`;
  return `<!doctype html><meta charset="utf-8"><title>file icons 验收</title>
<style>
 body{background:#1c1a18;color:#f0ece6;font:13px/1.5 ui-monospace,Consolas,monospace;margin:16px}
 h1{font-size:15px;font-weight:600}
 .grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(150px,1fr));gap:10px}
 .cell{background:#252220;border-radius:6px;padding:8px;text-align:center}
 .pair{display:flex;justify-content:center;gap:10px;align-items:center}
 .pair svg{width:34px;height:34px}
 .pair span:nth-child(2){border-left:1px solid #4a443f;padding-left:10px}
 code{display:block;margin-top:4px;color:#9aa7b0;font-size:11px;word-break:break-all}
</style>
<h1>左 = 官方原图，右 = 展平重建（共 ${entries.length} 枚）。两侧不一致的就是管线丢了东西。</h1>
<div class="grid">${entries.map(cell).join('')}</div>`;
}

/** 把展平结果拼回一张 SVG —— 只为在预览页 / 验收脚本里和原图对照。 */
export function rebuildSvg(art) {
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

// ─────────────────────────── 主流程 ───────────────────────────

function main() {
  const wantPreview = process.argv.includes('--preview');
  /** 指纹 → { idx, art } */
  const seen = new Map();
  const arts = [];
  const allWarnings = [];
  const previewEntries = [];

  /** 取一枚图并登记进 arts，返回下标。 */
  const register = (key, kind, iconName) => {
    const svg = fetchIcon(key, kind);
    const art = flatten(svg, `${kind}:${key}`);
    const fp = fingerprint(art);
    const label = kind === 'file' ? key : `${key}/${kind === 'folder-open' ? ' 开' : ''}`;
    const hit = seen.get(fp);
    if (hit !== undefined) {
      arts[hit].keys.push(label);
      return hit;
    }
    const idx = arts.length;
    if (idx > 0xffff) throw new Error('图标数超过 u16 上限');
    art.name = iconName;
    art.keys = [label];
    arts.push(art);
    seen.set(fp, idx);
    if (art.warnings.length) allWarnings.push({ key: `${kind}:${key}`, name: iconName, warnings: [...new Set(art.warnings)] });
    if (wantPreview) previewEntries.push({ label: `${iconName}\n(${kind}:${key})`, original: svg, rebuilt: rebuildSvg(art) });
    return idx;
  };

  // 兜底先注册，好让「与兜底同图」的 key 自然落到兜底上（下面靠下标相等剔除）
  const fallbackFile = register('__mini_term_unknown__.__none__', 'file', 'file');
  const fallbackFolder = register('__mini_term_unknown_folder__', 'folder', 'folder');
  const fallbackFolderOpen = register('__mini_term_unknown_folder__', 'folder-open', 'folder-open');

  const nameFor = (key, kind) => `${kind === 'file' ? '' : 'folder-'}${key.replace(/^\./, 'dot-').replace(/[^a-zA-Z0-9]+/g, '-')}`;

  // 清单项可以是 `'rs'`，也可以是 `['sqlite3', 'sqlite']`（后者是借图，见 EXTENSIONS 注释）
  const keyOf = (e) => (Array.isArray(e) ? e[0] : e);
  const sampleOf = (e) => (Array.isArray(e) ? e[1] : e);
  const exact = EXACT_NAMES.map((e) => [
    keyOf(e),
    register(sampleOf(e), 'file', nameFor(keyOf(e), 'file')),
  ]);
  const prefix = NAME_PREFIXES.map(([p, sample]) => [p, register(sample, 'file', nameFor(p, 'file'))]);
  const ext = EXTENSIONS.map((e) => [
    keyOf(e),
    register(`a.${sampleOf(e)}`, 'file', `ext-${keyOf(e)}`),
  ]);
  const folders = FOLDER_NAMES.map((d) => [
    d,
    [register(d, 'folder', nameFor(d, 'folder')), register(d, 'folder-open', `${nameFor(d, 'folder')}-open`)],
  ]);

  // 冗余条目一律剔掉：一条规则只有在「删了它结果会变」时才值得留在表里。
  // `Cargo.lock` 与 `.lock` 拿的是同一枚锁图，那 EXACT 里那条就是白占位；
  // 没有专属图的目录同理，查不到本来就落兜底。
  const extOf = (key) => {
    const i = key.lastIndexOf('.');
    return i > 0 && i + 1 < key.length ? key.slice(i + 1) : null;
  };
  const trimmedExt = ext.filter(([, i]) => i !== fallbackFile);
  const extIdx = new Map(trimmedExt);
  const prefixIdx = prefix.slice().sort((a, b) => b[0].length - a[0].length);
  /** 假装表里没有这条整名规则，它会落到哪枚图上。 */
  const withoutExact = (key) => {
    const pre = prefixIdx.find(([p]) => key.startsWith(p));
    if (pre) return pre[1];
    const e = extOf(key);
    return (e && extIdx.get(e)) ?? fallbackFile;
  };
  const trimmedExact = exact.filter(([key, i]) => i !== withoutExact(key));
  const trimmedPrefix = prefix.filter(([key, i]) => {
    const e = extOf(key);
    return i !== ((e && extIdx.get(e)) ?? fallbackFile);
  });
  const trimmedFolders = folders.filter(([, [c, o]]) => c !== fallbackFolder || o !== fallbackFolderOpen);

  // 剔完冗余条目后，可能有图标不再被任何 key 指向 —— 压实一遍，别把死数据写进 Rust
  const used = new Set([fallbackFile, fallbackFolder, fallbackFolderOpen]);
  for (const [, i] of [...trimmedExact, ...trimmedPrefix, ...trimmedExt]) used.add(i);
  for (const [, [c, o]] of trimmedFolders) {
    used.add(c);
    used.add(o);
  }
  const remap = new Map();
  const live = [];
  arts.forEach((art, i) => {
    if (!used.has(i)) return;
    remap.set(i, live.length);
    live.push(art);
  });
  const orphans = arts.length - live.length;
  const mapOne = ([k, i]) => [k, remap.get(i)];
  const mapPair = ([k, [c, o]]) => [k, [remap.get(c), remap.get(o)]];

  const byKey = (a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0);
  const index = {
    exact: trimmedExact.map(mapOne).sort(byKey),
    // 前缀表按长度倒序：`.env` 与 `.env.` 这种包含关系必须让长的先匹配
    prefix: trimmedPrefix.map(mapOne).sort((a, b) => b[0].length - a[0].length),
    ext: trimmedExt.map(mapOne).sort(byKey),
    folders: trimmedFolders.map(mapPair).sort(byKey),
    fallback: {
      file: remap.get(fallbackFile),
      folder: remap.get(fallbackFolder),
      folderOpen: remap.get(fallbackFolderOpen),
    },
  };

  fs.writeFileSync(outFile, emitRust(live, index));

  const dropped = {
    exact: exact.length - trimmedExact.length,
    ext: ext.length - trimmedExt.length,
    prefix: prefix.length - trimmedPrefix.length,
    folders: folders.length - trimmedFolders.length,
  };
  console.log(`✓ ${path.relative(process.cwd(), outFile)}`);
  console.log(`  图标 ${live.length} 枚（去重前 ${exact.length + prefix.length + ext.length + folders.length * 2} 次取图${orphans ? `，压掉 ${orphans} 枚没人指向的` : ''}）`);
  console.log(`  索引：整名 ${index.exact.length} / 前缀 ${index.prefix.length} / 扩展名 ${index.ext.length} / 目录 ${index.folders.length}`);
  console.log(`  冗余剔除（删了结果也不变）：整名 ${dropped.exact} / 前缀 ${dropped.prefix} / 扩展名 ${dropped.ext} / 目录 ${dropped.folders}`);
  console.log(`  源码 ${(fs.statSync(outFile).size / 1024).toFixed(0)} KB`);

  if (allWarnings.length) {
    console.log(`\n⚠ ${allWarnings.length} 枚图标有降级，逐条复核：`);
    for (const w of allWarnings) console.log(`  ${w.name.padEnd(26)} ${w.warnings.join('；')}`);
  }

  // 库压根没收录的 key —— 它们只能落通用图标。这是覆盖面的盲区，
  // 值得的可以在清单里配个别名（`.sqlite3` 借 `.sqlite` 的图）
  const blind = [
    ...exact.filter(([, i]) => i === fallbackFile).map(([k]) => k),
    ...ext.filter(([, i]) => i === fallbackFile).map(([k]) => `.${k}`),
  ];
  if (blind.length) {
    console.log(`\n· 库里没有专属图、落通用图标的 ${blind.length} 个 key：`);
    console.log(`  ${blind.join(' ')}`);
  }
  if (wantPreview) {
    fs.mkdirSync(path.dirname(previewFile), { recursive: true });
    fs.writeFileSync(previewFile, emitPreview(previewEntries));
    console.log(`\n✓ 预览页 ${path.relative(process.cwd(), previewFile)}`);
  }
}

// 被 verify_file_icons.mjs import 时不要顺手把文件重写一遍
if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main();
}
