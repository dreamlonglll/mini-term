// 中转协议一致性守门测试:校验 Rust (`relay-server/protocol/src/lib.rs`) 与
// TypeScript 手写镜像 (`mobile/src/protocol.ts`) 的版本号、消息联合体变体、
// 字段集合、字段可选性与枚举值完全对齐,避免任一侧改动后另一侧静默漂移。
//
// 刻意不引入 ts-rs 之类的代码生成:TS 侧是「手写镜像」(见 protocol.ts 文件头),
// 这里只用正则 + 括号配对解析两个协议文件里固定风格的定义。
//
// ── 字段可选性规则(按线上 JSON 的实际形状归纳,不按 Rust 类型字面猜) ──
//
// Rust 侧每个字段归纳出三个事实:
//   skip     = 带 `#[serde(skip_serializing_if = ...)]`:序列化时**键可能缺省**
//   nullable = `Option<T>` 且没有 `skip_serializing_if = "Option::is_none"`:
//              序列化时键在、**值可能是 null**(serde 对 None 默认写 null)
//   tolerant = `Option<T>` 或带 `#[serde(default)]`(含 `default = "..."`):
//              反序列化时**键缺省也接受**
// 只有 `#[serde(default)]`、没有 skip 的字段(如 `launchers` / `canStartSession`),
// Rust 序列化时**总会写出来**(空数组 / 空串 / false),`default` 只管反序列化
// (照顾旧桌面端不发),对移动端收到的 JSON 形状没有影响。
//
// 下行(RelayToMobile 及其承载的 DTO;中转对每条下行都是按 RelayToMobile 重新
// 序列化,不透传桌面端原文,见 relay-server/server 的 send_json),TS 描述的是收到的形状:
//   TS `?:`        ⇔ Rust skip        (键可能缺省 ⇔ 可选;反过来 TS 可选而 Rust 总会写出,
//                                        也算漂移——镜像就该和线上一模一样)
//   TS `| null`    ⇔ Rust nullable
// 上行(MobileToRelay),TS 是发送方,Rust 只管能不能收:
//   TS `?:`        ⇒ Rust tolerant     (TS 可能不发,Rust 必须接受缺省)
//   TS `| null`    ⇒ Rust `Option<T>`  (`#[serde(default)]` 的非 Option 字段收到 null 会报错)
//   Rust tolerant 而 TS 必填(如 requestProjectFiles.query)是允许的:发送方总会发。
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

const OPENERS = { '(': ')', '[': ']', '{': '}', '<': '>' };
const CLOSERS = new Set([')', ']', '}', '>']);

/** 按顶层逗号切分:`()` `[]` `{}` `<>` 与字符串字面量里的逗号不算(属性、泛型参数里都有逗号)。 */
function splitTopLevel(body) {
  const parts = [];
  let depth = 0;
  let inString = false;
  let start = 0;
  for (let i = 0; i < body.length; i += 1) {
    const ch = body[i];
    if (inString) {
      if (ch === '\\') i += 1;
      else if (ch === '"') inString = false;
      continue;
    }
    if (ch === '"') inString = true;
    else if (ch in OPENERS) depth += 1;
    else if (CLOSERS.has(ch)) depth -= 1;
    else if (ch === ',' && depth === 0) {
      parts.push(body.slice(start, i));
      start = i + 1;
    }
  }
  parts.push(body.slice(start));
  return parts.map((p) => p.trim()).filter(Boolean);
}

/** 剥出片段开头的 `#[...]` 属性(括号配对,属性里的字符串可含 `]`)。 */
function takeLeadingAttrs(segment) {
  const attrs = [];
  let rest = segment.trim();
  while (rest.startsWith('#[')) {
    let depth = 0;
    let inString = false;
    let end = -1;
    for (let i = 1; i < rest.length; i += 1) {
      const ch = rest[i];
      if (inString) {
        if (ch === '\\') i += 1;
        else if (ch === '"') inString = false;
        continue;
      }
      if (ch === '"') inString = true;
      else if (ch === '[') depth += 1;
      else if (ch === ']') {
        depth -= 1;
        if (depth === 0) {
          end = i;
          break;
        }
      }
    }
    assert.ok(end > 0, `unbalanced attribute in: ${rest.slice(0, 80)}`);
    attrs.push(rest.slice(0, end + 1));
    rest = rest.slice(end + 1).trim();
  }
  return { attrs, rest };
}

/** 这些 serde 属性会改变键名或字段去留,本测试的 camelCase 推导不再成立,遇到就要求先改测试。 */
const UNSUPPORTED_FIELD_SERDE = /\b(rename|alias|flatten|skip|skip_deserializing|skip_serializing|with|serialize_with|deserialize_with)\b(?!_)/;

/**
 * 解析一组 Rust 字段(结构体体或枚举结构体变体体),返回 Map<camelCase 名, 字段事实>。
 * `containerDefault`:容器上带 `#[serde(default)]` 时所有字段都 tolerant。
 */
function parseRustFieldList(body, where, containerDefault = false) {
  const fields = new Map();
  for (const segment of splitTopLevel(body)) {
    const { attrs, rest } = takeLeadingAttrs(segment);
    const m = /^(?:pub(?:\([^)]*\))?\s+)?([a-zA-Z_][a-zA-Z0-9_]*)\s*:\s*([\s\S]+)$/.exec(rest);
    assert.ok(m, `${where}: cannot parse Rust field from "${rest.slice(0, 80)}"`);
    const [, rawName, rawType] = m;
    const serde = attrs.filter((a) => /^#\[\s*serde\s*\(/.test(a)).join(' ');
    const serdeArgs = serde.replace(/skip_serializing_if\s*=\s*"[^"]*"/g, 'skip_serializing_if');
    assert.ok(
      !UNSUPPORTED_FIELD_SERDE.test(serdeArgs.replace(/skip_serializing_if/g, '')),
      `${where}.${rawName}: unsupported serde attribute ${serde}; extend relayProtocolParity.test.cjs first`,
    );
    const type = rawType.trim();
    const isOption = /^Option\s*</.test(type);
    const skip = /\bskip_serializing_if\b/.test(serde);
    const skipsNone = /skip_serializing_if\s*=\s*"Option::is_none"/.test(serde);
    const hasDefault = /(^|[(,\s])default\s*(=|,|\))/.test(serde.replace(/^#\[\s*serde\s*/, ' '));
    const name = snakeToCamel(rawName);
    assert.ok(!fields.has(name), `${where}: duplicate field ${name}`);
    fields.set(name, {
      type,
      isOption,
      skip,
      nullable: isOption && !skipsNone,
      tolerant: isOption || hasDefault || containerDefault,
    });
  }
  return fields;
}

/** `pub struct`/`pub enum` 定义前紧挨着的属性(derive / serde 容器属性)。 */
function containerAttrsBefore(clean, defIndex) {
  const before = clean.slice(0, defIndex);
  const m = /((?:\s*#\[[^\]]*\])*)\s*$/.exec(before);
  return m ? m[1] : '';
}

function findRustDef(src, kind, name) {
  const clean = stripComments(src);
  const re = new RegExp(`pub\\s+${kind}\\s+${name}\\s*\\{`);
  const match = re.exec(clean);
  assert.ok(match, `Rust ${kind} ${name} not found`);
  return {
    clean,
    match,
    body: extractBracedBlock(clean, match.index + match[0].length - 1),
    containerAttrs: containerAttrsBefore(clean, match.index),
  };
}

function parseRustStruct(src, structName) {
  const { body, containerAttrs } = findRustDef(src, 'struct', structName);
  assert.match(
    containerAttrs,
    /rename_all\s*=\s*"camelCase"/,
    `Rust struct ${structName} must be #[serde(rename_all = "camelCase")]`,
  );
  const containerDefault = /#\[\s*serde\s*\([^\]]*\bdefault\b/.test(containerAttrs);
  return parseRustFieldList(body, structName, containerDefault);
}

function parseRustUnitEnum(src, enumName) {
  const { body } = findRustDef(src, 'enum', enumName);
  assert.ok(!/serde\s*\([^)]*\brename\s*=/.test(body), `Rust enum ${enumName}: variant rename unsupported`);
  const variants = [];
  for (const part of stripRustAttrs(body).split(',')) {
    const trimmed = part.trim();
    if (!trimmed) continue;
    const m = /^([A-Z][A-Za-z0-9_]*)$/.exec(trimmed);
    assert.ok(m, `Rust enum ${enumName}: unexpected variant "${trimmed}"`);
    variants.push(pascalToCamel(m[1]));
  }
  return variants.sort();
}

/**
 * 解析 `#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]` 枚举。
 * 返回 Map<camelCase 变体名, Map<字段名, 字段事实>>。变体前的属性跳过,变体内字段的属性保留。
 */
function parseRustTaggedEnum(src, enumName) {
  const { body, containerAttrs } = findRustDef(src, 'enum', enumName);
  assert.match(containerAttrs, /tag\s*=\s*"type"/, `Rust enum ${enumName} must be tag = "type"`);
  assert.match(
    containerAttrs,
    /rename_all_fields\s*=\s*"camelCase"/,
    `Rust enum ${enumName} must be rename_all_fields = "camelCase"`,
  );

  const variants = new Map();
  for (const segment of splitTopLevel(body)) {
    const { attrs, rest } = takeLeadingAttrs(segment);
    assert.ok(
      !attrs.some((a) => /\brename\s*=/.test(a)),
      `${enumName}: variant-level rename unsupported (${rest.slice(0, 40)})`,
    );
    const idMatch = /^([A-Z][A-Za-z0-9_]*)\s*([\s\S]*)$/.exec(rest);
    assert.ok(idMatch, `${enumName}: cannot parse variant "${rest.slice(0, 60)}"`);
    const name = pascalToCamel(idMatch[1]);
    const tail = idMatch[2].trim();
    if (!tail) {
      variants.set(name, new Map());
      continue;
    }
    assert.equal(tail[0], '{', `${enumName}.${name}: only unit / struct variants are supported`);
    const inner = extractBracedBlock(tail, 0);
    variants.set(name, parseRustFieldList(inner, `${enumName}.${name}`));
  }
  return variants;
}

function findTsInterface(src, ifaceName) {
  const clean = stripComments(src);
  const re = new RegExp(`export\\s+interface\\s+${ifaceName}\\s*\\{`);
  const match = re.exec(clean);
  assert.ok(match, `TS interface ${ifaceName} not found`);
  return extractBracedBlock(clean, match.index + match[0].length - 1);
}

/** 返回 { tag, fields: Map<名, { optional, nullable, type }> }(`type` 字段单列为 tag)。 */
function parseTsInterface(src, ifaceName) {
  const body = findTsInterface(src, ifaceName);
  let tag = null;
  const fields = new Map();
  for (const rawLine of body.split(';')) {
    const line = rawLine.trim();
    if (!line) continue;
    const m = /^([a-zA-Z_][a-zA-Z0-9_]*)(\??)\s*:\s*([\s\S]+)$/.exec(line);
    assert.ok(m, `TS interface ${ifaceName}: cannot parse member "${line.slice(0, 60)}"`);
    const [, fieldName, question, rawType] = m;
    const fieldType = rawType.trim();
    if (fieldName === 'type') {
      const lit = /^['"]([a-zA-Z0-9_]+)['"]$/.exec(fieldType);
      assert.ok(lit, `TS interface ${ifaceName}.type should be a string literal, got ${fieldType}`);
      tag = lit[1];
    } else {
      fields.set(fieldName, {
        optional: question === '?',
        nullable: /(^|\|)\s*null\s*(\||$)/.test(fieldType),
        type: fieldType,
      });
    }
  }
  return { tag, fields };
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

const sortedKeys = (map) => [...map.keys()].sort();

/**
 * 下行:TS 描述收到的 JSON 形状,可选 ⇔ 键可能缺省,`| null` ⇔ 值可能为 null。
 * 返回全部不一致的描述(一次列全,不在第一处就停)。
 */
function downlinkOptionalityDrifts(where, rustFields, tsFields) {
  const drifts = [];
  for (const [name, rust] of rustFields) {
    const ts = tsFields.get(name);
    if (!ts) {
      drifts.push(`${where}.${name}: TS 侧缺少该字段`);
      continue;
    }
    if (ts.optional !== rust.skip) {
      drifts.push(
        rust.skip
          ? `${where}.${name}: Rust 带 skip_serializing_if(键可能缺省),TS 必须声明为 \`${name}?:\``
          : `${where}.${name}: Rust 总会序列化该键(${rust.type}),TS 不应声明为可选`,
      );
    }
    if (ts.nullable !== rust.nullable) {
      drifts.push(
        rust.nullable
          ? `${where}.${name}: Rust Option 未 skip_serializing_if,None 会序列化成 null,TS 须含 \`| null\``
          : `${where}.${name}: Rust 不会序列化出 null,TS 不应含 \`| null\``,
      );
    }
  }
  return drifts;
}

/** 上行:TS 是发送方,只要求 TS 可能不发的字段 Rust 能接受缺省、TS 可能发 null 的字段 Rust 是 Option。 */
function uplinkOptionalityDrifts(where, rustFields, tsFields) {
  const drifts = [];
  for (const [name, rust] of rustFields) {
    const ts = tsFields.get(name);
    if (!ts) {
      drifts.push(`${where}.${name}: TS 侧缺少该字段`);
      continue;
    }
    if (ts.optional && !rust.tolerant) {
      drifts.push(`${where}.${name}: TS 可选(可能不发),Rust 必须是 Option<T> 或带 #[serde(default)]`);
    }
    if (ts.nullable && !rust.isOption) {
      drifts.push(`${where}.${name}: TS 可能发 null,Rust 必须是 Option<T>`);
    }
  }
  return drifts;
}

// ---------------------------------------------------------------------------
// 断言
// ---------------------------------------------------------------------------

/** 两侧都有的共享 DTO 结构(全部只出现在下行消息里,见下方「DTO 只走下行」断言)。 */
const SHARED_DTOS = [
  'MobilePane',
  'MobileProject',
  'MobileLauncher',
  'MobileHistorySession',
  'MirrorMessage',
  'MirrorQuestionItem',
  'MirrorQuestionOption',
  'TerminalLine',
  'TerminalSpan',
  'MobileProjectFile',
  'MobileSkillItem',
];

/** 两侧都有的单元枚举(serde rename_all = camelCase ↔ TS 字符串字面量联合)。 */
const SHARED_ENUMS = [
  'MobileRejectReason',
  'CommandFailReason',
  'StartSessionFailReason',
  'HistoryFailReason',
  'AssistFailReason',
  'TerminalKey',
];

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
    sortedKeys(tsVariants),
    sortedKeys(rustVariants),
    'MobileToRelay message type tags drifted between Rust and TS',
  );
  for (const [tag, rustFields] of rustVariants) {
    assert.deepEqual(
      sortedKeys(tsVariants.get(tag)),
      sortedKeys(rustFields),
      `MobileToRelay.${tag} fields drifted between Rust and TS`,
    );
  }
});

test('中转协议:RelayToMobile 全部消息类型与字段对齐', () => {
  const rustVariants = parseRustTaggedEnum(rustSrc, 'RelayToMobile');
  const tsVariants = parseTsTaggedUnion(tsSrc, 'RelayToMobile');

  assert.deepEqual(
    sortedKeys(tsVariants),
    sortedKeys(rustVariants),
    'RelayToMobile message type tags drifted between Rust and TS',
  );
  for (const [tag, rustFields] of rustVariants) {
    assert.deepEqual(
      sortedKeys(tsVariants.get(tag)),
      sortedKeys(rustFields),
      `RelayToMobile.${tag} fields drifted between Rust and TS`,
    );
  }
});

test('中转协议:共享 DTO 结构字段对齐', () => {
  for (const name of SHARED_DTOS) {
    const rustFields = parseRustStruct(rustSrc, name);
    const tsIface = parseTsInterface(tsSrc, name);
    assert.equal(tsIface.tag, null, `DTO ${name} should not declare a tag field`);
    assert.deepEqual(
      sortedKeys(tsIface.fields),
      sortedKeys(rustFields),
      `DTO ${name} fields drifted between Rust and TS`,
    );
  }
});

test('中转协议:共享 DTO 只出现在下行消息里(可选性按下行规则校验的前提)', () => {
  const { body } = findRustDef(rustSrc, 'enum', 'MobileToRelay');
  for (const name of SHARED_DTOS) {
    assert.ok(
      !new RegExp(`\\b${name}\\b`).test(body),
      `${name} 出现在 MobileToRelay 里:它同时走上行,需在本测试里补上行可选性校验`,
    );
  }
});

test('中转协议:下行消息与 DTO 的字段可选性与 JSON 形状对齐', () => {
  const rustVariants = parseRustTaggedEnum(rustSrc, 'RelayToMobile');
  const tsVariants = parseTsTaggedUnion(tsSrc, 'RelayToMobile');
  const drifts = [];
  for (const [tag, rustFields] of rustVariants) {
    drifts.push(...downlinkOptionalityDrifts(`RelayToMobile.${tag}`, rustFields, tsVariants.get(tag)));
  }
  for (const name of SHARED_DTOS) {
    drifts.push(
      ...downlinkOptionalityDrifts(name, parseRustStruct(rustSrc, name), parseTsInterface(tsSrc, name).fields),
    );
  }
  assert.deepEqual(drifts, [], `下行字段可选性漂移:\n${drifts.join('\n')}`);
});

test('中转协议:上行消息的 TS 可选字段 Rust 都能接受缺省', () => {
  const rustVariants = parseRustTaggedEnum(rustSrc, 'MobileToRelay');
  const tsVariants = parseTsTaggedUnion(tsSrc, 'MobileToRelay');
  const drifts = [];
  for (const [tag, rustFields] of rustVariants) {
    drifts.push(...uplinkOptionalityDrifts(`MobileToRelay.${tag}`, rustFields, tsVariants.get(tag)));
  }
  assert.deepEqual(drifts, [], `上行字段可选性漂移:\n${drifts.join('\n')}`);
});

test('中转协议:共享枚举值对齐', () => {
  for (const name of SHARED_ENUMS) {
    const rustValues = parseRustUnitEnum(rustSrc, name);
    const tsValues = parseTsStringUnion(tsSrc, name);
    assert.deepEqual(tsValues, rustValues, `Enum ${name} values drifted between Rust and TS`);
  }
});

// 解析器自检:规则本身写错时(比如 default 没认出来),上面的对账会整片误报或漏报
test('中转协议对账:Rust 字段属性解析自检', () => {
  const sample = `
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a_b: Option<String>,
    #[serde(default)]
    pub c: Vec<Foo>,
    pub d: Option<u64>,
    #[serde(default = "fallback")]
    pub e: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub f: bool,
    pub g: HashMap<String, Vec<u8>>
  `;
  const fields = parseRustFieldList(sample, 'Sample');
  assert.deepEqual(sortedKeys(fields), ['aB', 'c', 'd', 'e', 'f', 'g']);
  assert.deepEqual(
    Object.fromEntries(
      [...fields].map(([k, v]) => [k, { skip: v.skip, nullable: v.nullable, tolerant: v.tolerant }]),
    ),
    {
      aB: { skip: true, nullable: false, tolerant: true },
      c: { skip: false, nullable: false, tolerant: true },
      d: { skip: false, nullable: true, tolerant: true },
      e: { skip: false, nullable: false, tolerant: true },
      f: { skip: true, nullable: false, tolerant: false },
      g: { skip: false, nullable: false, tolerant: false },
    },
  );
  assert.throws(() => parseRustFieldList('#[serde(rename = "x")] pub y: u8', 'Bad'), /unsupported serde/);
});
