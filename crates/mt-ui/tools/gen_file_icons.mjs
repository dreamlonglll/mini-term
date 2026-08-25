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
import { getMaterialFileIcon, getMaterialFolderIcon } from '@baybreezy/file-extension-icon';
import {
  PRECISION, preprocess, flatten, rebuildSvg,
  fingerprint, rustIdent, f32, emitShapes,
} from './icon_pipeline.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
const outFile = path.resolve(here, '..', 'src', 'icons', 'file_art.rs');
const previewFile = path.resolve(here, '..', '..', '..', 'target', 'file-icons-preview.html');


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


// ─────────────────────────── 取图 ───────────────────────────

const dataUriToSvg = (uri) => Buffer.from(uri.split(',')[1], 'base64').toString('utf8');

export function fetchIcon(key, kind) {
  const raw = kind === 'file'
    ? getMaterialFileIcon(key)
    : getMaterialFolderIcon(key, kind === 'folder-open');
  return preprocess(dataUriToSvg(raw));
}

// verify_file_icons.mjs 要拿这两个去做「官方原图 vs 展平重建」的对照
export { flatten, rebuildSvg };


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
