const assert = require('node:assert/strict');
const { test } = require('node:test');

const {
  movePaneInLayout,
  movePaneToTabIndex,
  insertSplitAt,
  collectPanes,
} = require('../.tmp-tests/utils/layoutOps.js');

const pane = (id) => ({ id, shellName: 'zsh', status: 'idle' });
const leaf = (ids, activeId = ids[0]) => ({
  type: 'leaf',
  panes: ids.map(pane),
  activePaneId: activeId,
});
const split = (direction, children, sizes) => ({
  type: 'split',
  direction,
  children,
  sizes: sizes ?? children.map(() => 100 / children.length),
});

const paneIds = (node) => collectPanes(node).map((p) => p.id);

// ===== insertSplitAt =====

test('insertSplitAt before 把新 leaf 放在第一格', () => {
  const root = leaf(['a']);
  const next = insertSplitAt(root, 'a', 'horizontal', leaf(['b']), 'before');
  assert.equal(next.type, 'split');
  assert.deepEqual(next.children.map(paneIds), [['b'], ['a']]);
});

test('insertSplitAt after 保持原 leaf 在第一格（getNodeKey 稳定性前提）', () => {
  const root = leaf(['a']);
  const next = insertSplitAt(root, 'a', 'vertical', leaf(['b']), 'after');
  assert.deepEqual(next.children.map(paneIds), [['a'], ['b']]);
  assert.equal(next.direction, 'vertical');
});

// ===== movePaneInLayout：四边分屏落点 =====

test('拖到右侧：目标 leaf 右边分出新 leaf，源 leaf 塌陷', () => {
  const root = split('horizontal', [leaf(['a']), leaf(['b'])]);
  const next = movePaneInLayout(root, 'a', 'b', 'right');
  // a 的 leaf 空了 → 塌陷；b 处分裂为 [b, a]
  assert.deepEqual(paneIds(next), ['b', 'a']);
  assert.equal(next.type, 'split');
});

test('拖到上侧：新 leaf 在 before 位、方向 vertical', () => {
  const root = split('horizontal', [leaf(['a']), leaf(['b'])]);
  const next = movePaneInLayout(root, 'a', 'b', 'top');
  assert.equal(next.type, 'split');
  assert.equal(next.direction, 'vertical');
  assert.deepEqual(next.children.map(paneIds), [['a'], ['b']]);
});

test('center：并入目标 leaf 末尾并激活', () => {
  const root = split('horizontal', [leaf(['a']), leaf(['b', 'c'], 'c')]);
  const next = movePaneInLayout(root, 'a', 'b', 'center');
  assert.equal(next.type, 'leaf'); // 源塌陷后整棵树只剩一个 leaf
  assert.deepEqual(paneIds(next), ['b', 'c', 'a']);
  assert.equal(next.activePaneId, 'a');
});

test('center 拖回自己所在组是 no-op（返回 null）', () => {
  const root = split('horizontal', [leaf(['a', 'b']), leaf(['c'])]);
  assert.equal(movePaneInLayout(root, 'a', 'b', 'center'), null);
});

test('独占 leaf 的 pane 拖回自己身上四边也是 no-op', () => {
  const root = split('horizontal', [leaf(['a']), leaf(['b'])]);
  assert.equal(movePaneInLayout(root, 'a', 'a', 'left'), null);
});

test('锚点是被拖 pane 自己时换锚：从多 tab 组拆出去', () => {
  const root = leaf(['a', 'b'], 'a');
  const next = movePaneInLayout(root, 'a', 'a', 'right');
  assert.equal(next.type, 'split');
  assert.deepEqual(next.children.map(paneIds), [['b'], ['a']]);
});

test('移动不丢 pane：任意落点前后 pane 集合一致', () => {
  const root = split('vertical', [
    split('horizontal', [leaf(['a', 'b']), leaf(['c'])]),
    leaf(['d']),
  ]);
  for (const zone of ['left', 'right', 'top', 'bottom', 'center']) {
    const next = movePaneInLayout(root, 'a', 'd', zone);
    assert.deepEqual(paneIds(next).sort(), ['a', 'b', 'c', 'd'], `zone=${zone}`);
  }
});

// ===== movePaneToTabIndex：tab 栏按位落子 =====

test('同组重排：拖到右侧插入位左移补位', () => {
  const root = leaf(['a', 'b', 'c']);
  const next = movePaneToTabIndex(root, 'a', 'b', 2);
  assert.deepEqual(paneIds(next), ['b', 'a', 'c']);
  assert.equal(next.activePaneId, 'a');
});

test('同组重排：拖到最左 / 最右', () => {
  const root = leaf(['a', 'b', 'c']);
  assert.deepEqual(paneIds(movePaneToTabIndex(root, 'c', 'a', 0)), ['c', 'a', 'b']);
  assert.deepEqual(paneIds(movePaneToTabIndex(root, 'a', 'b', 3)), ['b', 'c', 'a']);
});

test('同组重排：落回原位与紧邻右侧均为 no-op', () => {
  const root = leaf(['a', 'b', 'c']);
  assert.equal(movePaneToTabIndex(root, 'b', 'a', 1), null);
  assert.equal(movePaneToTabIndex(root, 'b', 'a', 2), null);
});

test('跨组按位插入：源 leaf 塌陷、按 index 落位并激活', () => {
  const root = split('horizontal', [leaf(['a']), leaf(['b', 'c'])]);
  const next = movePaneToTabIndex(root, 'a', 'b', 1);
  assert.equal(next.type, 'leaf');
  assert.deepEqual(paneIds(next), ['b', 'a', 'c']);
  assert.equal(next.activePaneId, 'a');
});

test('跨组插入 index 越界钳到末尾', () => {
  const root = split('horizontal', [leaf(['a']), leaf(['b'])]);
  const next = movePaneToTabIndex(root, 'a', 'b', 99);
  assert.deepEqual(paneIds(next), ['b', 'a']);
});

test('movePaneToTabIndex 不丢 pane', () => {
  const root = split('vertical', [leaf(['a', 'b']), leaf(['c', 'd'])]);
  for (let i = 0; i <= 2; i++) {
    const next = movePaneToTabIndex(root, 'a', 'c', i);
    assert.deepEqual(paneIds(next).sort(), ['a', 'b', 'c', 'd'], `index=${i}`);
  }
});
