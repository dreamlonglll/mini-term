// 中转协议一致性守门测试:校验 Rust (`relay-server/protocol/src/lib.rs`) 与
// TypeScript 手写镜像 (`mobile/src/protocol.ts`) 的版本号、消息联合体变体、
// 字段集合与枚举值完全对齐,避免任一侧改动后另一侧静默漂移。
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const projectRoot = path.resolve(__dirname, '..');
const rustPath = path.join(projectRoot, 'relay-server', 'protocol', 'src', 'lib.rs');
const tsPath = path.join(projectRoot, 'mobile', 'src', 'protocol.ts');

const rustSrc = fs.readFileSync(rustPath, 'utf8');
const tsSrc = fs.readFileSync(tsPath, 'utf8');

// ---------------------------------------------------------------------------
// 轻量词法/块提取工具(不依赖外部 parser,只解析协议文件内固定风格的定义)
// ---------------------------------------------------------------------------

function stripComments(src) {
  return src
    .replace(/\/\*[\s\S]*?\*\//g, '')
    .replace(/\/\/[^\n]*/g, '');
}

function snakeToCamel(name) {
  return name.replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());
}

function pascalToCamel(name) {
  return name.length ? name[0].toLowerCase() + name.slice(1) : name;
}

/** 从 `openBraceIdx` 处匹配成对 `{ ... }` 并返回大括号内部文本。 */
function extractBracedBlock(src, openBraceIdx) {
  assert.equal(src[openBraceIdx], '{', 'expected opening brace');
  let depth = 0;
  for (let i = openBraceIdx; i < src.length; i += 1) {
    const ch = src[i];
    if (ch === '{') depth += 1;
    else if (ch === '}') {
      depth -= 1;
      if (depth === 0) return src.slice(openBraceIdx + 1, i);
    }
  }
  throw new Error('unbalanced braces');
}

function stripRustAttrs(block) {
  return block.replace(/#\[[^\]]*\]/g, '');
}

function parseRustStructFields(src, structName) {
  const clean = stripComments(src);
  const re = new RegExp(`pub\\s+struct\\s+${structName}\\s*\\{`);
  const match = re.exec(clean);
  assert.ok(match, `Rust struct ${structName} not found`);
  const body = stripRustAttrs(extractBracedBlock(clean, match.index + match[0].length - 1));
  const fields = [];
  for (const part of body.split(',')) {
    const trimmed = part.trim();
    if (!trimmed) continue;
    const m = /^(?:pub\s+)?([a-zA-Z_][a-zA-Z0-9_]*)\s*:/.exec(trimmed);
    if (m) fields.push(snakeToCamel(m[1]));
  }
  return fields.sort();
}

function parseRustUnitEnum(src, enumName) {
  const clean = stripComments(src);
  const re = new RegExp(`pub\\s+enum\\s+${enumName}\\s*\\{`);
  const match = re.exec(clean);
  assert.ok(match, `Rust enum ${enumName} not found`);
  const body = stripRustAttrs(extractBracedBlock(clean, match.index + match[0].length - 1));
  const variants = [];
  for (const part of body.split(',')) {
    const trimmed = part.trim();
    if (!trimmed) continue;
    const m = /^([A-Z][A-Za-z0-9_]*)$/.exec(trimmed);
    if (m) variants.push(pascalToCamel(m[1]));
  }
  return variants.sort();
}

/** 解析 `#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]` 枚举。 */
function parseRustTaggedEnum(src, enumName) {
  const clean = stripComments(src);
  const re = new RegExp(`pub\\s+enum\\s+${enumName}\\s*\\{`);
  const match = re.exec(clean);
  assert.ok(match, `Rust tagged enum ${enumName} not found`);
  const body = stripRustAttrs(extractBracedBlock(clean, match.index + match[0].length - 1));

  const variants = new Map();
  let i = 0;
  while (i < body.length) {
    while (i < body.length && /[\s,]/.test(body[i])) i += 1;
    if (i >= body.length) break;
    const idMatch = /^([A-Z][A-Za-z0-9_]*)/.exec(body.slice(i));
    if (!idMatch) {
      i += 1;
      continue;
    }
    const name = pascalToCamel(idMatch[1]);
    i += idMatch[0].length;
    while (i < body.length && /\s/.test(body[i])) i += 1;
    if (body[i] === '{') {
      const inner = extractBracedBlock(body, i);
      i += inner.length + 2;
      const fields = [];
      for (const part of inner.split(',')) {
        const trimmed = part.trim();
        if (!trimmed) continue;
        const fm = /^([a-zA-Z_][a-zA-Z0-9_]*)\s*:/.exec(trimmed);
        if (fm) fields.push(snakeToCamel(fm[1]));
      }
      variants.set(name, fields.sort());
    } else {
      variants.set(name, []);
    }
  }
  return variants;
}

function parseTsInterface(src, ifaceName) {
  const clean = stripComments(src);
  const re = new RegExp(`export\\s+interface\\s+${ifaceName}\\s*\\{`);
  const match = re.exec(clean);
  assert.ok(match, `TS interface ${ifaceName} not found`);
  const body = extractBracedBlock(clean, match.index + match[0].length - 1);
  let tag = null;
  const fields = [];
  for (const rawLine of body.split(';')) {
    const line = rawLine.trim();
    if (!line) continue;
    const m = /^([a-zA-Z_][a-zA-Z0-9_]*)(\??)\s*:\s*([\s\S]+)$/.exec(line);
    if (!m) continue;
    const [, fieldName, , fieldType] = m;
    if (fieldName === 'type') {
      const lit = /^['"]([a-zA-Z0-9_]+)['"]$/.exec(fieldType.trim());
      assert.ok(lit, `TS interface ${ifaceName}.type should be a string literal, got ${fieldType}`);
      tag = lit[1];
    } else {
      fields.push(fieldName);
    }
  }
  return { tag, fields: fields.sort() };
}

function parseTsStringUnion(src, typeName) {
  const clean = stripComments(src);
  const re = new RegExp(`export\\s+type\\s+${typeName}\\s*=([\\s\\S]*?);`);
  const match = re.exec(clean);
  assert.ok(match, `TS union type ${typeName} not found`);
  const values = [];
  for (const m of match[1].matchAll(/['"]([a-zA-Z0-9_]+)['"]/g)) {
    values.push(m[1]);
  }
  return values.sort();
}

function parseTsTaggedUnion(src, unionName) {
  const clean = stripComments(src);
  const re = new RegExp(`export\\s+type\\s+${unionName}\\s*=([\\s\\S]*?);`);
  const match = re.exec(clean);
  assert.ok(match, `TS tagged union ${unionName} not found`);
  const members = match[1]
    .split('|')
    .map((s) => s.trim())
    .filter(Boolean);
  const variants = new Map();
  for (const member of members) {
    const { tag, fields } = parseTsInterface(src, member);
    assert.ok(tag, `TS union ${unionName} member ${member} is missing literal type tag`);
    assert.ok(!variants.has(tag), `Duplicate type tag '${tag}' in TS union ${unionName}`);
    variants.set(tag, fields);
  }
  return variants;
}

// ---------------------------------------------------------------------------
// 断言
// ---------------------------------------------------------------------------

test('中转协议:PROTOCOL_VERSION 在 Rust 与 TS 两侧一致', () => {
  const rustVer = /pub\s+const\s+PROTOCOL_VERSION\s*:\s*u32\s*=\s*(\d+)\s*;/.exec(rustSrc);
  const tsVer = /export\s+const\s+PROTOCOL_VERSION\s*=\s*(\d+)\s*;/.exec(tsSrc);
  assert.ok(rustVer, 'Rust PROTOCOL_VERSION not found');
  assert.ok(tsVer, 'TS PROTOCOL_VERSION not found');
  assert.equal(Number(tsVer[1]), Number(rustVer[1]));
});

test('中转协议:MobileToRelay 全部消息类型与字段对齐', () => {
  const rustVariants = parseRustTaggedEnum(rustSrc, 'MobileToRelay');
  const tsVariants = parseTsTaggedUnion(tsSrc, 'MobileToRelay');

  assert.deepEqual(
    [...tsVariants.keys()].sort(),
    [...rustVariants.keys()].sort(),
    'MobileToRelay message type tags drifted between Rust and TS',
  );
  for (const [tag, rustFields] of rustVariants) {
    assert.deepEqual(
      tsVariants.get(tag),
      rustFields,
      `MobileToRelay.${tag} fields drifted between Rust and TS`,
    );
  }
});

test('中转协议:RelayToMobile 全部消息类型与字段对齐', () => {
  const rustVariants = parseRustTaggedEnum(rustSrc, 'RelayToMobile');
  const tsVariants = parseTsTaggedUnion(tsSrc, 'RelayToMobile');

  assert.deepEqual(
    [...tsVariants.keys()].sort(),
    [...rustVariants.keys()].sort(),
    'RelayToMobile message type tags drifted between Rust and TS',
  );
  for (const [tag, rustFields] of rustVariants) {
    assert.deepEqual(
      tsVariants.get(tag),
      rustFields,
      `RelayToMobile.${tag} fields drifted between Rust and TS`,
    );
  }
});

test('中转协议:共享 DTO 结构字段对齐', () => {
  const structs = [
    'MobilePane',
    'MobileProject',
    'MobileLauncher',
    'MobileHistorySession',
    'MirrorMessage',
    'MirrorQuestionItem',
    'MirrorQuestionOption',
  ];
  for (const name of structs) {
    const rustFields = parseRustStructFields(rustSrc, name);
    const tsIface = parseTsInterface(tsSrc, name);
    assert.equal(tsIface.tag, null, `DTO ${name} should not declare a tag field`);
    assert.deepEqual(tsIface.fields, rustFields, `DTO ${name} fields drifted between Rust and TS`);
  }
});

test('中转协议:共享失败原因枚举值对齐', () => {
  const enums = [
    'MobileRejectReason',
    'CommandFailReason',
    'StartSessionFailReason',
    'HistoryFailReason',
  ];
  for (const name of enums) {
    const rustValues = parseRustUnitEnum(rustSrc, name);
    const tsValues = parseTsStringUnion(tsSrc, name);
    assert.deepEqual(tsValues, rustValues, `Enum ${name} values drifted between Rust and TS`);
  }
});
