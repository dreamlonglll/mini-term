// crate 依赖分层的守门测试:把 AGENTS.md 与各 Cargo.toml 注释里写明的「铁律」落成断言,
// 违规时报出哪个 crate、经哪条依赖链引入了什么、违反了哪条规则。
//
// 两类规则:
// 1. 外部依赖白名单 —— **只**给明文写了「依赖表只许有 …」的 crate。其它 crate 加第三方
//    依赖是正常开发(例如 mt-project 加编码探测库),不在这里拦。
// 2. 分层禁区 —— 某 crate 的依赖闭包里不许出现某些包(工作区内的上层 crate、gpui 等)。
//
// 判定口径与 .github/scripts/non_gpui_packages.mjs 相同:直接依赖只看 normal / build
// (dev 依赖不链进产物);闭包 = 该 crate 自己的 normal / build / dev 依赖 + 传递依赖的
// normal / build 依赖。不按平台过滤(全平台一起看),平台专属依赖靠规则里的 cfg 放行。
const assert = require('node:assert/strict');
const { execFileSync } = require('node:child_process');
const path = require('node:path');
const test = require('node:test');

const projectRoot = path.resolve(__dirname, '..');

// ---------------------------------------------------------------------------
// 规则
// ---------------------------------------------------------------------------

/** 外部依赖白名单:直接 normal / build 依赖必须全在 `allow` 里(按包名)。 */
const DIRECT_ALLOWLISTS = [
  {
    crate: 'mt-core',
    allow: ['serde', 'serde_json', 'dirs'],
    rule: 'mt-core 是叶子,只许依赖 serde / serde_json / dirs',
    source: 'AGENTS.md crate 表 mt-core 行 + crates/mt-core/Cargo.toml(它被三个 sidecar 直接链接)',
  },
  {
    crate: 'mt-secret',
    allow: ['mt-core', 'ring', 'base64', 'serde', 'serde_json', 'zeroize'],
    // 只在这个 cfg 下才许出现的依赖
    allowOnCfg: { 'windows-sys': 'cfg(windows)' },
    rule: 'mt-secret 只许依赖 mt-core / ring / base64 / serde / serde_json / zeroize(Windows 另许 windows-sys)',
    source: 'AGENTS.md crate 表 mt-secret 行(它经 mt-ssh 进入三个 sidecar)',
  },
  {
    crate: 'mt-theme-packs',
    allow: ['anyhow', 'serde', 'serde_json', 'sha2', 'zip'],
    rule: 'mt-theme-packs 只许依赖 anyhow / serde / serde_json / sha2 / zip',
    source: 'AGENTS.md crate 表 mt-theme-packs 行 + 其 Cargo.toml(mt-ui 依赖它,一重 mt-ui 就跟着重编)',
  },
  {
    crate: 'mt-i18n',
    allow: ['serde'],
    rule: 'mt-i18n 零第三方依赖,只许有可选的 serde',
    source: 'crates/mt-i18n/Cargo.toml「唯一(且可选)的依赖」+ src/lib.rs「零第三方依赖」',
  },
];

/** 分层禁区:`crate` 的依赖闭包里不许出现 `ban` 里的任何包。 */
const CLOSURE_BANS = [
  {
    crate: 'mt-ui',
    ban: ['mt-config', 'mt-i18n', 'mt-project'],
    rule: 'mt-ui 不许依赖 mt-config / mt-i18n / mt-project',
    source: 'AGENTS.md crate 表 mt-ui 行 + crates/mt-ui/Cargo.toml(前两者改得勤、后者带 git2,一依赖就连带重编两万多行)',
  },
  {
    crate: 'mt-terminal',
    ban: ['gpui-pre'],
    rule: 'mt-terminal 不许依赖 gpui',
    source: 'AGENTS.md crate 表 mt-terminal 行「不依赖 gpui」',
  },
  {
    crate: 'mt-config',
    ban: ['gpui-pre'],
    rule: 'mt-config 不许依赖 gpui',
    source: 'AGENTS.md crate 表 mt-config 行「不依赖 gpui」',
  },
  {
    crate: 'mt-remote',
    ban: ['gpui-pre'],
    rule: 'mt-remote 不许依赖 gpui',
    source: 'AGENTS.md crate 表 mt-remote 行「不依赖 gpui」+ crates/mt-remote/Cargo.toml',
  },
];

/**
 * 工作区内部的边:`crate` 依赖闭包里出现的工作区成员只许是 `allowMembers`。
 * mt-ssh 与 sidecar 共用,它再拉别的工作区 crate 就会经 sidecars/ 工作区进小二进制。
 */
const MEMBER_ALLOWLISTS = [
  {
    crate: 'mt-ssh',
    allowMembers: ['mt-core', 'mt-secret'],
    rule: 'mt-ssh 在工作区内只许依赖 mt-core / mt-secret',
    source: 'AGENTS.md crate 表 mt-ssh / mt-secret 行(主程序与三个 sidecar 共用这一层)',
  },
];

/** sidecars/ 独立工作区:只许从主工作区链接这几个 crate,且不许带 SQLite / gpui。 */
const SIDECAR_RULE = {
  root: 'mt-sidecars',
  allowLocal: ['mt-core', 'mt-ssh', 'mt-secret'],
  ban: ['rusqlite', 'gpui-pre'],
  rule: '三个 sidecar 只许链接 mt-core / mt-ssh(及其下的 mt-secret),不许带进 rusqlite / gpui',
  source: 'AGENTS.md「config.json 为什么还在」一节 + 根 Cargo.toml rusqlite / mt-layout 注释',
};

// ---------------------------------------------------------------------------
// 依赖图
// ---------------------------------------------------------------------------

function cargoMetadata(manifestPath) {
  return JSON.parse(
    execFileSync('cargo', ['metadata', '--format-version', '1', '--manifest-path', manifestPath], {
      cwd: projectRoot,
      encoding: 'utf8',
      maxBuffer: 256 * 1024 * 1024,
    }),
  );
}

/** 把 `cargo metadata` 的结果包成按包名查询的依赖图。 */
function buildGraph(meta) {
  const pkgById = new Map(meta.packages.map((p) => [p.id, p]));
  const nodes = new Map(meta.resolve.nodes.map((n) => [n.id, n]));
  const members = new Set(meta.workspace_members);
  const memberIdByName = new Map(meta.workspace_members.map((id) => [pkgById.get(id).name, id]));
  const nameOf = (id) => pkgById.get(id)?.name ?? id;

  /** id 的依赖边;kinds 是允许的依赖种类集合(normal 记作 'normal')。 */
  function edges(id, kinds) {
    const node = nodes.get(id);
    if (!node) return [];
    return node.deps
      .map((d) => ({
        pkg: d.pkg,
        kinds: d.dep_kinds.filter((k) => kinds.has(k.kind ?? 'normal')),
      }))
      .filter((d) => d.kinds.length > 0);
  }

  /** 从 rootId 出发的依赖闭包,返回 Map<id, 父 id>(用于还原依赖链)。 */
  function closure(rootId) {
    const parent = new Map();
    const queue = [];
    for (const d of edges(rootId, new Set(['normal', 'build', 'dev']))) {
      if (!parent.has(d.pkg)) {
        parent.set(d.pkg, rootId);
        queue.push(d.pkg);
      }
    }
    while (queue.length) {
      const id = queue.shift();
      for (const d of edges(id, new Set(['normal', 'build']))) {
        if (d.pkg !== rootId && !parent.has(d.pkg)) {
          parent.set(d.pkg, id);
          queue.push(d.pkg);
        }
      }
    }
    return parent;
  }

  function chain(parent, rootId, id) {
    const names = [nameOf(id)];
    let cur = parent.get(id);
    while (cur && cur !== rootId) {
      names.push(nameOf(cur));
      cur = parent.get(cur);
    }
    names.push(nameOf(rootId));
    return names.reverse().join(' → ');
  }

  return { pkgById, members, memberIdByName, nameOf, edges, closure, chain };
}

function requireMember(graph, crate) {
  const id = graph.memberIdByName.get(crate);
  if (!id) {
    return { id: null, error: `规则里的 crate ${crate} 不在工作区成员里(改名或删除了?规则要跟着改)` };
  }
  return { id, error: null };
}

// ---------------------------------------------------------------------------
// 判定(纯函数,输入依赖图,输出违规描述;自检用例直接喂构造的图)
// ---------------------------------------------------------------------------

function checkDirectAllowlists(graph, rules) {
  const out = [];
  for (const r of rules) {
    const { id, error } = requireMember(graph, r.crate);
    if (error) {
      out.push(error);
      continue;
    }
    for (const d of graph.edges(id, new Set(['normal', 'build']))) {
      const dep = graph.nameOf(d.pkg);
      if (r.allow.includes(dep)) continue;
      const cfg = r.allowOnCfg?.[dep];
      if (cfg && d.kinds.every((k) => k.target === cfg)) continue;
      const how = d.kinds
        .map((k) => `${k.kind ?? 'normal'}${k.target ? ` @ ${k.target}` : ''}`)
        .join(', ');
      out.push(
        `${r.crate} 直接依赖了 ${dep}(${how}),不在白名单内。` +
          `违反规则:${r.rule}。来源:${r.source}`,
      );
    }
  }
  return out;
}

function checkClosureBans(graph, rules) {
  const out = [];
  for (const r of rules) {
    const { id, error } = requireMember(graph, r.crate);
    if (error) {
      out.push(error);
      continue;
    }
    const parent = graph.closure(id);
    for (const depId of parent.keys()) {
      const dep = graph.nameOf(depId);
      if (!r.ban.includes(dep)) continue;
      out.push(
        `${r.crate} 引入了 ${dep}(依赖链 ${graph.chain(parent, id, depId)})。` +
          `违反规则:${r.rule}。来源:${r.source}`,
      );
    }
  }
  return out;
}

function checkMemberAllowlists(graph, rules) {
  const out = [];
  for (const r of rules) {
    const { id, error } = requireMember(graph, r.crate);
    if (error) {
      out.push(error);
      continue;
    }
    const parent = graph.closure(id);
    for (const depId of parent.keys()) {
      if (!graph.members.has(depId)) continue;
      const dep = graph.nameOf(depId);
      if (r.allowMembers.includes(dep)) continue;
      out.push(
        `${r.crate} 引入了工作区 crate ${dep}(依赖链 ${graph.chain(parent, id, depId)})。` +
          `违反规则:${r.rule}。来源:${r.source}`,
      );
    }
  }
  return out;
}

/** sidecars 工作区:本地 path 包(source 为 null 且不是 sidecar 自己)只许是白名单里的。 */
function checkSidecars(graph, r) {
  const { id, error } = requireMember(graph, r.root);
  if (error) return [error];
  const out = [];
  const parent = graph.closure(id);
  for (const depId of parent.keys()) {
    const pkg = graph.pkgById.get(depId);
    const dep = graph.nameOf(depId);
    const local = pkg && pkg.source === null;
    if ((local && !r.allowLocal.includes(dep)) || r.ban.includes(dep)) {
      out.push(
        `${r.root} 引入了 ${dep}(依赖链 ${graph.chain(parent, id, depId)})。` +
          `违反规则:${r.rule}。来源:${r.source}`,
      );
    }
  }
  return out;
}

/** 整张图里只许有一份 gpui:不许出现 gpui-ce / crates.io 的 gpui,gpui-pre 只许一个版本。 */
function checkSingleGpui(meta) {
  const out = [];
  const inGraph = new Set(meta.resolve.nodes.map((n) => n.id));
  const pkgs = meta.packages.filter((p) => inGraph.has(p.id));
  for (const p of pkgs) {
    if (p.name === 'gpui-ce' || p.name === 'gpui') {
      out.push(
        `依赖图里出现了 ${p.name} ${p.version}。违反规则:只许用 gpui-pre 这一份 gpui,` +
          '混用会出现两个类型不互通的 gpui crate。来源:根 Cargo.toml「不要换成 gpui-ce」',
      );
    }
  }
  const versions = pkgs.filter((p) => p.name === 'gpui-pre').map((p) => p.version);
  if (versions.length > 1) {
    out.push(
      `依赖图里有 ${versions.length} 个版本的 gpui-pre(${versions.join(', ')})。` +
        '违反规则:只许一份 gpui。来源:根 Cargo.toml GPUI 栈注释',
    );
  }
  return out;
}

// ---------------------------------------------------------------------------
// 用例
// ---------------------------------------------------------------------------

let rootMeta;
let rootGraph;
function root() {
  if (!rootGraph) {
    rootMeta = cargoMetadata('Cargo.toml');
    rootGraph = buildGraph(rootMeta);
  }
  return rootGraph;
}

function assertClean(violations) {
  assert.equal(violations.length, 0, `\n${violations.join('\n')}`);
}

test('分层:有铁律的 crate 外部依赖不出白名单', () => {
  assertClean(checkDirectAllowlists(root(), DIRECT_ALLOWLISTS));
});

test('分层:禁区里的包不进依赖闭包', () => {
  assertClean(checkClosureBans(root(), CLOSURE_BANS));
});

test('分层:与 sidecar 共用的 mt-ssh 只依赖 mt-core / mt-secret', () => {
  assertClean(checkMemberAllowlists(root(), MEMBER_ALLOWLISTS));
});

test('分层:依赖图里只有一份 gpui', () => {
  root();
  assertClean(checkSingleGpui(rootMeta));
});

test('分层:sidecar 工作区只链接 mt-core / mt-ssh / mt-secret', () => {
  const graph = buildGraph(cargoMetadata(path.join('sidecars', 'Cargo.toml')));
  assertClean(checkSidecars(graph, SIDECAR_RULE));
});

// 判定逻辑自检:构造一张带违规的小图,每条规则都得报出来(防判定逻辑写错后永远绿)。
test('分层:判定逻辑能抓到构造的违规', () => {
  const pkg = (name, source = null) => ({ id: `${name} 0.1.0`, name, version: '0.1.0', source });
  const reg = 'registry+https://github.com/rust-lang/crates.io-index';
  const packages = [
    pkg('mt-core'),
    pkg('mt-ui'),
    pkg('mt-glue'),
    pkg('mt-config'),
    pkg('mt-secret'),
    pkg('serde', reg),
    pkg('anyhow', reg),
    pkg('windows-sys', reg),
    pkg('gpui-ce', reg),
  ];
  const dep = (to, kind = null, target = null) => ({
    pkg: `${to} 0.1.0`,
    dep_kinds: [{ kind, target }],
  });
  const meta = {
    packages,
    workspace_members: ['mt-core 0.1.0', 'mt-ui 0.1.0', 'mt-glue 0.1.0', 'mt-config 0.1.0', 'mt-secret 0.1.0'],
    resolve: {
      nodes: [
        { id: 'mt-core 0.1.0', deps: [dep('serde'), dep('anyhow')] },
        { id: 'mt-ui 0.1.0', deps: [dep('mt-glue')] },
        { id: 'mt-glue 0.1.0', deps: [dep('mt-config')] },
        { id: 'mt-config 0.1.0', deps: [] },
        // windows-sys 不在 cfg(windows) 下 → 违规;cfg(windows) 下的那条放行
        { id: 'mt-secret 0.1.0', deps: [dep('windows-sys', null, 'cfg(unix)'), dep('gpui-ce')] },
        { id: 'serde 0.1.0', deps: [] },
        { id: 'anyhow 0.1.0', deps: [] },
        { id: 'windows-sys 0.1.0', deps: [] },
        { id: 'gpui-ce 0.1.0', deps: [] },
      ],
    },
  };
  const graph = buildGraph(meta);

  const direct = checkDirectAllowlists(graph, DIRECT_ALLOWLISTS.slice(0, 2));
  assert.ok(direct.some((m) => m.startsWith('mt-core 直接依赖了 anyhow')), direct.join('\n'));
  assert.ok(direct.some((m) => m.startsWith('mt-secret 直接依赖了 windows-sys')), direct.join('\n'));
  assert.ok(!direct.some((m) => m.includes('依赖了 serde')), '白名单里的 serde 不该报');

  const bans = checkClosureBans(graph, CLOSURE_BANS.slice(0, 1));
  assert.deepEqual(bans.length, 1, bans.join('\n'));
  assert.match(bans[0], /mt-ui 引入了 mt-config\(依赖链 mt-ui → mt-glue → mt-config\)/);
  assert.match(bans[0], /违反规则:mt-ui 不许依赖/);

  const missing = checkClosureBans(graph, [{ crate: 'mt-gone', ban: [], rule: '', source: '' }]);
  assert.match(missing[0], /mt-gone 不在工作区成员里/);

  assert.equal(checkSingleGpui(meta).length, 1);

  // cfg(windows) 下的 windows-sys 放行
  const ok = buildGraph({
    ...meta,
    resolve: {
      nodes: meta.resolve.nodes.map((n) =>
        n.id === 'mt-secret 0.1.0' ? { ...n, deps: [dep('windows-sys', null, 'cfg(windows)')] } : n,
      ),
    },
  });
  assert.deepEqual(
    checkDirectAllowlists(ok, DIRECT_ALLOWLISTS.slice(1, 2)),
    [],
  );
});
