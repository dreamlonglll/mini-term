const assert = require('node:assert/strict');
const test = require('node:test');

const {
  AGENT_BRANCH_CAPS,
  branchCapsForAgent,
  mergeLineageEdges,
  buildSessionTree,
  findFamilyRoot,
  flattenSessionTree,
} = require('../.tmp-tests/utils/sessionBranch.js');

const S = (id, timestamp = '2026-08-14T10:00:00Z', sessionType = 'claude') => ({
  id, sessionType, title: `t-${id}`, timestamp,
});
const E = (sessionId, parentSessionId, agent = 'claude') => ({ agent, sessionId, parentSessionId });

test('能力位：claude/codex 出命令，非法 id 与未知 agent 拒绝', () => {
  assert.equal(
    AGENT_BRANCH_CAPS.claude.forkCommand('abc-123'),
    'claude --resume abc-123 --fork-session',
  );
  assert.equal(AGENT_BRANCH_CAPS.codex.forkCommand('019f'), 'codex fork 019f');
  assert.equal(AGENT_BRANCH_CAPS.codex.resumeCommand('019f'), 'codex resume 019f');
  // 注入向量:空格/分号/引号一律不出命令
  assert.equal(AGENT_BRANCH_CAPS.claude.forkCommand('a b'), null);
  assert.equal(AGENT_BRANCH_CAPS.claude.resumeCommand('x;rm -rf'), null);
  // agent 归一化:与 buildResumeCommand 同口径 —— hook 上报的是 'claude-code',
  // 必须落到 claude 能力位(这里回归的是「严格匹配把入口藏掉」的实测 bug);
  // 缺省/未知按 claude,grok/opencode/pi 显式无能力位
  assert.ok(branchCapsForAgent(undefined));
  assert.equal(
    branchCapsForAgent('claude-code').forkCommand('abc'),
    'claude --resume abc --fork-session',
  );
  assert.ok(branchCapsForAgent('Codex'));
  assert.equal(branchCapsForAgent('grok'), null);
  assert.equal(branchCapsForAgent('opencode'), null);
  assert.equal(branchCapsForAgent('pi'), null);
});

test('mergeLineageEdges：按 child 去重且磁盘优先', () => {
  const disk = [E('c1', 'p-disk')];
  const kept = [E('c1', 'p-book'), E('c2', 'p2')];
  const merged = mergeLineageEdges(disk, kept);
  const byChild = new Map(merged.map((e) => [e.sessionId, e]));
  assert.equal(byChild.get('c1').parentSessionId, 'p-disk');
  assert.equal(byChild.get('c2').parentSessionId, 'p2');
});

test('buildSessionTree：多层嵌套、子按时间升序、根保持输入序', () => {
  const sessions = [
    S('root2', '2026-08-14T12:00:00Z'),
    S('root1', '2026-08-14T10:00:00Z'),
    S('b', '2026-08-14T11:00:00Z'),
    S('a', '2026-08-14T10:30:00Z'),
    S('a1', '2026-08-14T10:45:00Z'),
  ];
  const edges = [E('a', 'root1'), E('b', 'root1'), E('a1', 'a')];
  const roots = buildSessionTree(sessions, edges);
  assert.deepEqual(roots.map((r) => r.session.id), ['root2', 'root1']);
  const root1 = roots[1];
  assert.deepEqual(root1.children.map((c) => c.session.id), ['a', 'b']); // 时间升序
  assert.deepEqual(root1.children[0].children.map((c) => c.session.id), ['a1']);
  assert.equal(root1.depth, 0);
  assert.equal(root1.children[0].depth, 1);
  assert.equal(root1.children[0].children[0].depth, 2);
  assert.equal(root1.children[0].edge.parentSessionId, 'root1');
});

test('悬空父落为根，环防御不死循环', () => {
  const sessions = [S('x'), S('y')];
  // x 的父不在列表;y 与 x 互指成环
  const dangling = buildSessionTree(sessions, [E('x', 'ghost')]);
  assert.deepEqual(dangling.map((r) => r.session.id), ['x', 'y']);
  const cyclic = buildSessionTree(sessions, [E('x', 'y'), E('y', 'x')]);
  // 环内所有边按悬空处理:两个都还在(都是根),谁也不消失
  assert.equal(cyclic.length, 2);
});

test('自指边忽略；findFamilyRoot 命中任意层级', () => {
  const sessions = [S('r'), S('c1', '2026-08-14T11:00:00Z')];
  const roots = buildSessionTree(sessions, [E('r', 'r'), E('c1', 'r')]);
  assert.equal(roots.length, 1);
  assert.equal(findFamilyRoot(roots, 'c1').session.id, 'r');
  assert.equal(findFamilyRoot(roots, 'r').session.id, 'r');
  assert.equal(findFamilyRoot(roots, 'nope'), null);
});

test('flattenSessionTree：先根 DFS，连线前缀正确（├ └ │ 与留白）', () => {
  const sessions = [
    S('r', '2026-08-14T10:00:00Z'),
    S('a', '2026-08-14T10:10:00Z'),
    S('a1', '2026-08-14T10:20:00Z'),
    S('b', '2026-08-14T10:30:00Z'),
  ];
  // r ├── a（a 下还有 a1）└── b：a 非末子 → a1 行的上层画 │
  const rows = flattenSessionTree(buildSessionTree(sessions, [E('a', 'r'), E('a1', 'a'), E('b', 'r')]));
  assert.deepEqual(
    rows.map((x) => [x.prefix, x.node.session.id]),
    [
      ['', 'r'],
      ['├─ ', 'a'],
      ['│  └─ ', 'a1'],
      ['└─ ', 'b'],
    ],
  );
  // 末子的后代:上层留白不画 │
  const rows2 = flattenSessionTree(buildSessionTree(sessions, [E('a', 'r'), E('b', 'a'), E('a1', 'b')]));
  assert.deepEqual(rows2.map((x) => x.prefix), ['', '└─ ', '   └─ ', '      └─ ']);
});
