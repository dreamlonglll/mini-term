#!/usr/bin/env node
// 按需扩展的打包脚本(release.yml 的 remote-git-helpers / package-release 作业调用;
// 本地往返见 tests/packExtensions.test.cjs 与 crates/mt-extensions/tests/fixtures/)。
//
// 只用 Node 22 内置的 zlib / crypto / fs,零 npm 依赖。产物**逐字节可复现**:zip 条目的
// mtime 固定为 1980-01-01 00:00、权限位固定(可执行 0755 / 其余 0644)、条目按命令行
// 给出的顺序排列 —— 同一份输入到哪台机器上打都是同一个 sha256。
//
// 子命令:
//
//   pack     --id <id> (--exe <文件> | --file <文件>)... --out-dir <目录>
//            把若干文件打成一个扩展 zip(deflate),资产名内容寻址:
//            `mt-ext_<id>_<zip sha256 前 12 位>.zip`;同目录写一份描述片段
//            `<id>.fragment.json`({id, asset, sha256, size, files[]}),并打印到 stdout。
//            `--exe` 标记可执行(Unix 上装成 0755),`--file` 不标。
//
//   manifest --platform <key> --app-version <版本> --tag <tag> [--offline-bundle <资产名>]
//            --fragments <目录或文件>... [--assets <目录>] --out <extensions.json>
//            汇总片段写清单(schema 1,组件按 id 排序)。给了 `--assets` 就逐个核对片段
//            引用的 zip 在场、字节数与 sha256 对得上。
//
//   bundle   --manifest <extensions.json> --assets <目录> --out <离线包.zip>
//            打离线扩展包:清单引用的全部扩展 zip 原样 store(不再压)+ 一份
//            `extensions.json` 副本(**导入时不信任这份副本**,只认本机清单)。
//
//   verify   --manifest <extensions.json> --assets <目录>
//            核对清单引用的每个扩展 zip:在场、字节数 / sha256 对得上、条目恰好是清单列的
//            那几个文件且每个文件的字节数 / sha256 对得上(与 mt-extensions 的安装校验同口径)。
//
// 口径与 crates/mt-extensions(Rust 侧安装代码)对齐的几条:id 只许已知集合;文件名单段、
// 无分隔符、无 `..`、不以 `.` 开头;单文件与 zip 都不超过 64 MB。

import { createHash } from 'node:crypto';
import { mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { basename, join, resolve } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { deflateRawSync, inflateRawSync } from 'node:zlib';

export const SCHEMA_VERSION = 1;
export const KNOWN_IDS = Object.freeze([
  'git-helper-linux-x86_64',
  'git-helper-linux-aarch64',
  'git-helper-macos-x86_64',
  'git-helper-macos-aarch64',
  'ssh-tools',
  'syntax-pack',
]);
export const PLATFORMS = Object.freeze(['windows-x64', 'macos-arm64', 'macos-x64', 'linux-x64']);
export const MAX_FILE_BYTES = 64 * 1024 * 1024;
export const MAX_ZIP_BYTES = 64 * 1024 * 1024;
const MANIFEST_FILE = 'extensions.json';
const FRAGMENT_SUFFIX = '.fragment.json';

// ─── 基础件 ──────────────────────────────────────────────────

export function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

export function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

/** 与 Rust 侧 `is_safe_file_name` 同口径。 */
export function isSafeFileName(name) {
  if (typeof name !== 'string' || name.length === 0 || Buffer.byteLength(name) > 128) return false;
  if (name.startsWith('.') || name.endsWith('.') || name.endsWith(' ') || name.includes('..')) return false;
  // eslint-disable-next-line no-control-regex
  if (/[\u0000-\u001f\u007f/\\:<>"|?*]/.test(name)) return false;
  const stem = name.split('.')[0].toUpperCase();
  return !(/^(CON|PRN|AUX|NUL)$/.test(stem) || /^(COM|LPT)[0-9]$/.test(stem));
}

export function assetName(id, zipSha256) {
  return `mt-ext_${id}_${zipSha256.slice(0, 12)}.zip`;
}

function fail(message) {
  throw new Error(`[pack-extensions] ${message}`);
}

function requireId(id) {
  if (!KNOWN_IDS.includes(id)) fail(`未知扩展 id: ${id}(可选 ${KNOWN_IDS.join(' / ')})`);
}

function requireLabel(field, value) {
  if (typeof value !== 'string' || !/^[A-Za-z0-9._+-]{1,64}$/.test(value)) {
    fail(`--${field} 不合法: ${JSON.stringify(value)}`);
  }
}

// ─── zip 写入(确定性) ───────────────────────────────────────

/** DOS 时间 00:00:00 / 日期 1980-01-01(zip 能表达的最早时刻)。 */
const DOS_TIME = 0;
const DOS_DATE = (0 << 9) | (1 << 5) | 1;
const MADE_BY_UNIX = (3 << 8) | 20;
const S_IFREG = 0o100000;

/**
 * 写一个 zip。entries: [{ name, data, executable, method: 'deflate' | 'store' }]。
 * deflate 后反而更大的条目退回 store(规则固定,结果仍确定)。
 */
export function writeZip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const entry of entries) {
    const name = Buffer.from(entry.name, 'utf8');
    const data = entry.data;
    let method = 0;
    let payload = data;
    if (entry.method !== 'store') {
      const deflated = deflateRawSync(data, { level: 9 });
      if (deflated.length < data.length) {
        method = 8;
        payload = deflated;
      }
    }
    const crc = crc32(data);
    const flags = /[^\x00-\x7f]/.test(entry.name) ? 0x0800 : 0;
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4);
    local.writeUInt16LE(flags, 6);
    local.writeUInt16LE(method, 8);
    local.writeUInt16LE(DOS_TIME, 10);
    local.writeUInt16LE(DOS_DATE, 12);
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(payload.length, 18);
    local.writeUInt32LE(data.length, 22);
    local.writeUInt16LE(name.length, 26);
    local.writeUInt16LE(0, 28);
    locals.push(local, name, payload);

    const mode = S_IFREG | (entry.executable ? 0o755 : 0o644);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(MADE_BY_UNIX, 4);
    central.writeUInt16LE(20, 6);
    central.writeUInt16LE(flags, 8);
    central.writeUInt16LE(method, 10);
    central.writeUInt16LE(DOS_TIME, 12);
    central.writeUInt16LE(DOS_DATE, 14);
    central.writeUInt32LE(crc, 16);
    central.writeUInt32LE(payload.length, 20);
    central.writeUInt32LE(data.length, 24);
    central.writeUInt16LE(name.length, 28);
    central.writeUInt16LE(0, 30);
    central.writeUInt16LE(0, 32);
    central.writeUInt16LE(0, 34);
    central.writeUInt16LE(0, 36);
    central.writeUInt32LE((mode << 16) >>> 0, 38);
    central.writeUInt32LE(offset, 42);
    centrals.push(central, name);
    offset += local.length + name.length + payload.length;
  }
  const centralSize = centrals.reduce((sum, part) => sum + part.length, 0);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(0, 4);
  end.writeUInt16LE(0, 6);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(centralSize, 12);
  end.writeUInt32LE(offset, 16);
  end.writeUInt16LE(0, 20);
  const zip = Buffer.concat([...locals, ...centrals, end]);
  if (offset > 0xffffffff || entries.length > 0xffff) fail('zip64 不支持');
  return zip;
}

/** 读回本脚本写的(及常规的非 zip64)zip:[{ name, data, mode }]。只给 verify / 测试用。 */
export function readZip(zip) {
  const endAt = zip.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  if (endAt < 0) fail('不是 zip(没有中央目录结尾记录)');
  const count = zip.readUInt16LE(endAt + 10);
  let at = zip.readUInt32LE(endAt + 16);
  const out = [];
  for (let index = 0; index < count; index++) {
    if (zip.readUInt32LE(at) !== 0x02014b50) fail('中央目录损坏');
    const method = zip.readUInt16LE(at + 10);
    const crc = zip.readUInt32LE(at + 16);
    const compressed = zip.readUInt32LE(at + 20);
    const size = zip.readUInt32LE(at + 24);
    const nameLength = zip.readUInt16LE(at + 28);
    const extraLength = zip.readUInt16LE(at + 30);
    const commentLength = zip.readUInt16LE(at + 32);
    const mode = zip.readUInt32LE(at + 38) >>> 16;
    const localAt = zip.readUInt32LE(at + 42);
    const name = zip.subarray(at + 46, at + 46 + nameLength).toString('utf8');
    const localName = zip.readUInt16LE(localAt + 26);
    const localExtra = zip.readUInt16LE(localAt + 28);
    const start = localAt + 30 + localName + localExtra;
    const raw = zip.subarray(start, start + compressed);
    let data;
    if (method === 0) data = Buffer.from(raw);
    else if (method === 8) data = inflateRawSync(raw);
    else fail(`条目 ${name} 用了不支持的压缩方式 ${method}`);
    if (data.length !== size || crc32(data) !== crc) fail(`条目 ${name} 的大小或 CRC 不符`);
    out.push({ name, data, mode });
    at += 46 + nameLength + extraLength + commentLength;
  }
  return out;
}

// ─── 子命令 ──────────────────────────────────────────────────

/**
 * 打一个扩展 zip。files: [{ path, executable }](文件名取 basename)。
 * 返回 { fragment, zip, asset }。
 */
export function packComponent(id, files) {
  requireId(id);
  if (!files.length) fail(`${id}: 至少要一个文件(--exe / --file)`);
  const seen = new Set();
  const entries = files.map(({ path, executable }) => {
    const stat = statSync(path);
    if (!stat.isFile()) fail(`${path} 不是常规文件`);
    if (stat.size > MAX_FILE_BYTES) fail(`${path} 超过 ${MAX_FILE_BYTES} 字节`);
    const name = basename(path);
    if (!isSafeFileName(name)) fail(`文件名不合规: ${name}`);
    const key = name.toLowerCase();
    if (seen.has(key)) fail(`${id}: 文件名重复(不分大小写): ${name}`);
    seen.add(key);
    const data = readFileSync(path);
    return { name, data, executable: Boolean(executable), method: 'deflate' };
  });
  const zip = writeZip(entries);
  if (zip.length > MAX_ZIP_BYTES) fail(`${id}: zip 超过 ${MAX_ZIP_BYTES} 字节`);
  const digest = sha256(zip);
  const asset = assetName(id, digest);
  const fragment = {
    id,
    asset,
    sha256: digest,
    size: zip.length,
    files: entries.map((entry) => ({
      name: entry.name,
      sha256: sha256(entry.data),
      size: entry.data.length,
      executable: entry.executable,
    })),
  };
  return { fragment, zip, asset };
}

function validateFragment(fragment, source) {
  const where = `片段 ${source}`;
  if (!fragment || typeof fragment !== 'object') fail(`${where}: 不是对象`);
  requireId(fragment.id);
  if (!/^[0-9a-f]{64}$/.test(fragment.sha256)) fail(`${where}: sha256 不合规`);
  if (!Number.isSafeInteger(fragment.size) || fragment.size <= 0 || fragment.size > MAX_ZIP_BYTES) {
    fail(`${where}: size 不合规`);
  }
  if (fragment.asset !== assetName(fragment.id, fragment.sha256)) fail(`${where}: 资产名不是内容寻址的`);
  if (!Array.isArray(fragment.files) || !fragment.files.length) fail(`${where}: files 为空`);
  for (const file of fragment.files) {
    if (!isSafeFileName(file.name)) fail(`${where}: 文件名不合规 ${file.name}`);
    if (!/^[0-9a-f]{64}$/.test(file.sha256)) fail(`${where}: ${file.name} 的 sha256 不合规`);
    if (!Number.isSafeInteger(file.size) || file.size < 0 || file.size > MAX_FILE_BYTES) {
      fail(`${where}: ${file.name} 的 size 不合规`);
    }
    if (typeof file.executable !== 'boolean') fail(`${where}: ${file.name} 的 executable 不是布尔值`);
  }
  return {
    id: fragment.id,
    asset: fragment.asset,
    sha256: fragment.sha256,
    size: fragment.size,
    files: fragment.files.map(({ name, sha256: digest, size, executable }) => ({
      name,
      sha256: digest,
      size,
      executable,
    })),
  };
}

/** 收集片段:参数可以是片段文件,也可以是目录(取其中全部 `*.fragment.json`)。 */
export function readFragments(sources) {
  const files = [];
  for (const source of sources) {
    const stat = statSync(source);
    if (stat.isDirectory()) {
      for (const name of readdirSync(source).sort()) {
        if (name.endsWith(FRAGMENT_SUFFIX)) files.push(join(source, name));
      }
    } else {
      files.push(source);
    }
  }
  if (!files.length) fail(`没有找到任何 *${FRAGMENT_SUFFIX}: ${sources.join(', ')}`);
  return files.map((file) => validateFragment(JSON.parse(readFileSync(file, 'utf8')), file));
}

/** 核对一个扩展 zip 与清单组件(与 mt-extensions 的安装校验同口径)。 */
export function verifyComponentZip(component, zip) {
  if (zip.length !== component.size) fail(`${component.asset}: ${zip.length} 字节,清单说 ${component.size}`);
  if (sha256(zip) !== component.sha256) fail(`${component.asset}: sha256 不符`);
  const entries = readZip(zip);
  if (entries.length !== component.files.length) {
    fail(`${component.asset}: 条目数 ${entries.length},清单列了 ${component.files.length} 个文件`);
  }
  const seen = new Set();
  for (const entry of entries) {
    const spec = component.files.find((file) => file.name === entry.name);
    if (!spec) fail(`${component.asset}: 多余条目 ${entry.name}`);
    if (seen.has(entry.name)) fail(`${component.asset}: 重复条目 ${entry.name}`);
    seen.add(entry.name);
    const kind = entry.mode & 0o170000;
    if (kind !== 0 && kind !== S_IFREG) fail(`${component.asset}: ${entry.name} 不是常规文件`);
    if (entry.data.length !== spec.size || sha256(entry.data) !== spec.sha256) {
      fail(`${component.asset}: ${entry.name} 的字节数或 sha256 与清单不符`);
    }
  }
}

export function buildManifest({ platform, appVersion, tag, offlineBundle, fragments, assetsDir }) {
  if (!PLATFORMS.includes(platform)) fail(`未知平台键: ${platform}(可选 ${PLATFORMS.join(' / ')})`);
  requireLabel('app-version', appVersion);
  requireLabel('tag', tag);
  if (offlineBundle !== undefined && !isSafeFileName(offlineBundle)) fail(`--offline-bundle 不合规: ${offlineBundle}`);
  const components = [...fragments].sort((a, b) => a.id.localeCompare(b.id));
  for (let index = 1; index < components.length; index++) {
    if (components[index].id === components[index - 1].id) fail(`扩展 ${components[index].id} 出现了两次`);
  }
  if (assetsDir) {
    for (const component of components) {
      verifyComponentZip(component, readFileSync(join(assetsDir, component.asset)));
    }
  }
  const manifest = { schema: SCHEMA_VERSION, appVersion, tag, platform };
  if (offlineBundle !== undefined) manifest.offlineBundle = offlineBundle;
  manifest.components = components;
  return manifest;
}

export function serializeManifest(manifest) {
  return `${JSON.stringify(manifest, null, 2)}\n`;
}

/** 读回一份清单,按 Rust 侧同样的规矩校验组件。 */
export function readManifest(path) {
  const manifest = JSON.parse(readFileSync(path, 'utf8'));
  if (manifest.schema !== SCHEMA_VERSION) fail(`${path}: schema 不是 ${SCHEMA_VERSION}`);
  if (!Array.isArray(manifest.components)) fail(`${path}: components 不是数组`);
  manifest.components = manifest.components.map((component) => validateFragment(component, path));
  return manifest;
}

export function buildBundle(manifestPath, assetsDir) {
  const manifest = readManifest(manifestPath);
  const entries = manifest.components.map((component) => {
    const zip = readFileSync(join(assetsDir, component.asset));
    verifyComponentZip(component, zip);
    return { name: component.asset, data: zip, executable: false, method: 'store' };
  });
  entries.push({
    name: MANIFEST_FILE,
    data: readFileSync(manifestPath),
    executable: false,
    method: 'store',
  });
  return writeZip(entries);
}

export function verifyAssets(manifestPath, assetsDir) {
  const manifest = readManifest(manifestPath);
  for (const component of manifest.components) {
    verifyComponentZip(component, readFileSync(join(assetsDir, component.asset)));
  }
  return manifest.components.length;
}

// ─── 命令行 ──────────────────────────────────────────────────

function parseArgs(argv) {
  const options = new Map();
  const files = [];
  for (let index = 0; index < argv.length; index++) {
    const flag = argv[index];
    if (!flag.startsWith('--')) fail(`多余的参数: ${flag}`);
    const value = argv[index + 1];
    if (value === undefined || value.startsWith('--')) fail(`${flag} 缺少取值`);
    index++;
    if (flag === '--exe' || flag === '--file') {
      files.push({ path: resolve(value), executable: flag === '--exe' });
    } else if (flag === '--fragments') {
      options.set(flag, [...(options.get(flag) ?? []), resolve(value)]);
    } else {
      if (options.has(flag)) fail(`${flag} 只能给一次`);
      options.set(flag, value);
    }
  }
  return { options, files };
}

function required(options, flag) {
  const value = options.get(flag);
  if (value === undefined) fail(`缺少 ${flag}`);
  return value;
}

function main(argv) {
  const [command, ...rest] = argv;
  const { options, files } = parseArgs(rest);
  switch (command) {
    case 'pack': {
      const id = required(options, '--id');
      const outDir = resolve(required(options, '--out-dir'));
      const { fragment, zip, asset } = packComponent(id, files);
      mkdirSync(outDir, { recursive: true });
      writeFileSync(join(outDir, asset), zip);
      writeFileSync(join(outDir, `${id}${FRAGMENT_SUFFIX}`), `${JSON.stringify(fragment, null, 2)}\n`);
      console.log(JSON.stringify(fragment));
      console.error(`[pack-extensions] ${id}: ${asset} (${zip.length} bytes)`);
      return;
    }
    case 'manifest': {
      const manifest = buildManifest({
        platform: required(options, '--platform'),
        appVersion: required(options, '--app-version'),
        tag: required(options, '--tag'),
        offlineBundle: options.get('--offline-bundle'),
        fragments: readFragments(required(options, '--fragments')),
        assetsDir: options.has('--assets') ? resolve(options.get('--assets')) : undefined,
      });
      const out = resolve(required(options, '--out'));
      writeFileSync(out, serializeManifest(manifest));
      console.error(`[pack-extensions] ${out}: ${manifest.components.length} components (${manifest.platform})`);
      return;
    }
    case 'bundle': {
      const out = resolve(required(options, '--out'));
      const zip = buildBundle(resolve(required(options, '--manifest')), resolve(required(options, '--assets')));
      writeFileSync(out, zip);
      console.error(`[pack-extensions] ${out}: ${zip.length} bytes`);
      return;
    }
    case 'verify': {
      const count = verifyAssets(resolve(required(options, '--manifest')), resolve(required(options, '--assets')));
      console.error(`[pack-extensions] verified ${count} extension assets`);
      return;
    }
    default:
      fail('用法: node scripts/pack-extensions.mjs <pack|manifest|bundle|verify> [参数](见文件头注释)');
  }
}

const invokedPath = process.argv[1]
  ? pathToFileURL(fileURLToPath(pathToFileURL(process.argv[1]))).href
  : null;
if (invokedPath === import.meta.url) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }
}
