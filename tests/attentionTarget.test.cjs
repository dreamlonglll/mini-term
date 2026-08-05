const assert = require('node:assert/strict');

const { pickAttentionTarget } = require('../.tmp-tests/utils/attentionTarget.js');

/** 造一个只有一层 leaf 的项目布局 */
function project(panes) {
  return { layout: { type: 'leaf', panes, activeIndex: 0 } };
}

const states = (entries) => new Map(entries);
const doneOrder = (entries) => new Map(entries);

// 待确认压过一切:哪怕已完成/处理中排在更前面的项目里,也先去等你拍板的那个
{
  const target = pickAttentionTarget(
    states([
      ['p1', project([{ id: 'a', status: 'ai-working' }, { id: 'b', status: 'ai-idle' }])],
      ['p2', project([{ id: 'c', status: 'ai-idle', attention: true }])],
    ]),
    doneOrder([['b', 1]]),
  );
  assert.deepEqual(target, { projectId: 'p2', paneId: 'c' });
}

// error 与 attention 同档 —— 异常同样是「不处理就推进不了」
{
  const target = pickAttentionTarget(
    states([['p1', project([{ id: 'a', status: 'ai-working' }, { id: 'b', status: 'error' }])]]),
    doneOrder([]),
  );
  assert.deepEqual(target, { projectId: 'p1', paneId: 'b' });
}

// 已完成取「最先完成」的那个,而不是遍历序里第一个撞上的
{
  const target = pickAttentionTarget(
    states([
      ['p1', project([{ id: 'a', status: 'ai-idle' }])],
      ['p2', project([{ id: 'b', status: 'ai-idle' }])],
    ]),
    doneOrder([['a', 7], ['b', 3]]),
  );
  assert.deepEqual(target, { projectId: 'p2', paneId: 'b' });
}

// 已完成优先于处理中:跑完的在等你,还在跑的不需要你
{
  const target = pickAttentionTarget(
    states([
      ['p1', project([{ id: 'a', status: 'ai-working' }])],
      ['p2', project([{ id: 'b', status: 'ai-idle' }])],
    ]),
    doneOrder([['b', 1]]),
  );
  assert.deepEqual(target, { projectId: 'p2', paneId: 'b' });
}

// 只剩处理中时才落到处理中
{
  const target = pickAttentionTarget(
    states([['p1', project([{ id: 'a', status: 'idle' }, { id: 'b', status: 'ai-working' }])]]),
    doneOrder([]),
  );
  assert.deepEqual(target, { projectId: 'p1', paneId: 'b' });
}

// 又开始工作的 pane 仍带着完成序号时按完成算(与标题栏状态灯口径一致)
{
  const target = pickAttentionTarget(
    states([['p1', project([{ id: 'a', status: 'ai-working' }])]]),
    doneOrder([['a', 2]]),
  );
  assert.deepEqual(target, { projectId: 'p1', paneId: 'a' });
}

// onlyProjectId:状态栏右键菜单点某个项目,只在它内部挑,不被别的项目的黄灯抢走
{
  const all = states([
    ['p1', project([{ id: 'a', status: 'ai-idle', attention: true }])],
    ['p2', project([{ id: 'b', status: 'ai-working' }, { id: 'c', status: 'ai-idle' }])],
  ]);
  const done = doneOrder([['c', 5]]);
  assert.deepEqual(pickAttentionTarget(all, done), { projectId: 'p1', paneId: 'a' });
  assert.deepEqual(pickAttentionTarget(all, done, 'p2'), { projectId: 'p2', paneId: 'c' });
}

// 限定的项目全闲 / 不存在 → null,调用方据此退回「只切项目」
{
  const all = states([
    ['p1', project([{ id: 'a', status: 'ai-working' }])],
    ['p2', project([{ id: 'b', status: 'idle' }])],
  ]);
  assert.equal(pickAttentionTarget(all, doneOrder([]), 'p2'), null);
  assert.equal(pickAttentionTarget(all, doneOrder([]), 'nope'), null);
}

// 没有布局的项目(还没开过终端)跳过,不该在这里抛
{
  const target = pickAttentionTarget(
    states([['p1', { layout: null }], ['p2', project([{ id: 'b', status: 'ai-working' }])]]),
    doneOrder([]),
  );
  assert.deepEqual(target, { projectId: 'p2', paneId: 'b' });
}

// 全都安静 → null
{
  assert.equal(
    pickAttentionTarget(states([['p1', project([{ id: 'a', status: 'idle' }])]]), doneOrder([])),
    null,
  );
}

console.log('attentionTarget: all assertions passed');
