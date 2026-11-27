// 移动端 PWA 的纯逻辑单测(零依赖):直接 import `mobile/src` 下零运行时依赖的 .ts 模块,
// 靠 Node 自带的 type stripping(Node ≥ 22.18 / 23.6 默认开启)跑,不引入测试框架与构建步骤。
//
// 覆盖:
// - relay/mirrorMerge.ts   重订阅快照按 seq 合并 / 追加去重 / 分页前插
// - relay/commandSlot.ts   指令单槽状态机(回执、超时 / 断线 / 桌面掉线 → 结果未知、迟到回执升级、重连复位)
// - navStack.ts            History API 同步规划(含一个模拟浏览器历史栈的端到端推演)
// - pullGesture.ts         下拉刷新判顶 / 横滑放弃 / 阻尼
const assert = require('node:assert/strict');
const path = require('node:path');
const test = require('node:test');
const { pathToFileURL } = require('node:url');

const skip =
  process.features && process.features.typescript
    ? false
    : '当前 Node 不支持 type stripping(需要 Node ≥ 22.18 / 23.6),跳过移动端纯逻辑单测';

const srcRoot = path.resolve(__dirname, '..', 'mobile', 'src');
const load = (rel) => import(pathToFileURL(path.join(srcRoot, rel)).href);

function msg(seq, extra = {}) {
  return { seq, source: 'assistant', content: `m${seq}`, timestamp: `t${seq}`, ...extra };
}
const range = (from, to, extra) => {
  const out = [];
  for (let s = from; s <= to; s += 1) out.push(msg(s, extra));
  return out;
};
const seqs = (messages) => messages.map((m) => m.seq);

// ---------------------------------------------------------------------------
// mirrorMerge
// ---------------------------------------------------------------------------

test('镜像合并:首个快照 / 空快照 / 快照覆盖全部已有 → 按快照替换', { skip }, async () => {
  const { mergeMirrorSnapshot } = await load('relay/mirrorMerge.ts');
  const snap = range(10, 19);
  assert.deepEqual(mergeMirrorSnapshot([], false, snap, true), {
    messages: snap,
    hasMore: true,
    merged: false,
  });
  assert.deepEqual(mergeMirrorSnapshot(range(0, 9), false, [], false), {
    messages: [],
    hasMore: false,
    merged: false,
  });
  const covered = mergeMirrorSnapshot(range(15, 19), true, range(10, 19), true);
  assert.equal(covered.merged, false);
  assert.deepEqual(seqs(covered.messages), seqs(range(10, 19)));
  assert.equal(covered.hasMore, true, '没保留更早部分时 hasMore 取快照的');
});

test('镜像合并:同一会话重订阅保留已加载的更早历史,快照覆盖它所含的区间', { skip }, async () => {
  const { mergeMirrorSnapshot } = await load('relay/mirrorMerge.ts');
  // 已「加载更早」到 seq 20,断线期间又来了 10 条;重订阅快照是最近一页 50..79
  const prev = range(20, 69);
  const snap = range(50, 79);
  const out = mergeMirrorSnapshot(prev, true, snap, true);
  assert.equal(out.merged, true);
  assert.deepEqual(seqs(out.messages), seqs(range(20, 79)), '有序、无重复、无缺口');
  assert.equal(out.hasMore, true, '保留了更早部分:沿用旧 hasMore');
  assert.equal(out.messages[0], prev[0], '更早部分沿用旧对象');
  assert.equal(out.messages[30], prev[30], '重叠区逐字段相同的也沿用旧对象(seq 50)');
  assert.equal(out.messages[59], snap[29], '新增部分用快照对象');

  // 已经加载到第一条(旧 hasMore=false):合并后也不该再出「加载更早」
  const toStart = mergeMirrorSnapshot(range(0, 69), false, range(50, 79), true);
  assert.equal(toStart.merged, true);
  assert.equal(toStart.hasMore, false);
});

test('镜像合并:重订阅时来源被重新改标,仍算同一条消息并以快照为准', { skip }, async () => {
  const { mergeMirrorSnapshot } = await load('relay/mirrorMerge.ts');
  const prev = range(0, 9);
  const snap = range(5, 12).map((m) => (m.seq === 6 ? { ...m, source: 'mobile' } : m));
  const out = mergeMirrorSnapshot(prev, false, snap, true);
  assert.equal(out.merged, true);
  assert.deepEqual(seqs(out.messages), seqs(range(0, 12)));
  assert.equal(out.messages[6].source, 'mobile', '快照覆盖它所含的区间');
  assert.equal(out.messages[6], snap[1]);
});

test('镜像合并:判不准是同一条时间线时整体替换(换绑 / 变短 / 缺口 / 恰好首尾相接)', { skip }, async () => {
  const { mergeMirrorSnapshot } = await load('relay/mirrorMerge.ts');
  // 换绑:seq 重新从 0 编号,同 seq 内容不同
  const rebound = range(0, 5).map((m) => ({ ...m, content: `new-${m.seq}` }));
  const r1 = mergeMirrorSnapshot(range(0, 30), false, rebound, false);
  assert.equal(r1.merged, false);
  assert.deepEqual(r1.messages, rebound);
  // 旧列表里有比快照最新一条还新的消息
  const r2 = mergeMirrorSnapshot(range(0, 30), false, range(10, 20), true);
  assert.equal(r2.merged, false);
  assert.deepEqual(seqs(r2.messages), seqs(range(10, 20)));
  // 断线期间新增超过一页:中间有缺口
  const r3 = mergeMirrorSnapshot(range(0, 30), false, range(60, 90), true);
  assert.equal(r3.merged, false);
  assert.equal(r3.hasMore, true);
  // 恰好首尾相接:没有重叠可核对,宁可丢已加载的历史也不拼接
  const r4 = mergeMirrorSnapshot(range(0, 30), false, range(31, 60), true);
  assert.equal(r4.merged, false);
  // 时间戳不同也算不同消息(两段会话首条恰好同文)
  const r5 = mergeMirrorSnapshot(
    range(0, 10),
    false,
    range(0, 3).map((m) => ({ ...m, timestamp: 'other' })),
    false,
  );
  assert.equal(r5.merged, false);
});

test('镜像合并:乱序 / 重复 seq 的快照先规整再合并', { skip }, async () => {
  const { mergeMirrorSnapshot } = await load('relay/mirrorMerge.ts');
  const snap = [msg(12), msg(10), msg(11), msg(12)];
  const out = mergeMirrorSnapshot(range(0, 11), false, snap, true);
  assert.equal(out.merged, true);
  assert.deepEqual(seqs(out.messages), seqs(range(0, 12)));
});

test('镜像合并:实时追加去重、分页前插只收更早的', { skip }, async () => {
  const { appendMirrorMessages, prependOlderMirrorMessages } = await load('relay/mirrorMerge.ts');
  const existing = range(10, 14);
  const appended = appendMirrorMessages(existing, [msg(13), msg(15), msg(16)]);
  assert.deepEqual(seqs(appended), [10, 11, 12, 13, 14, 15, 16]);
  assert.equal(appendMirrorMessages(existing, [msg(14)]), existing, '没有新消息:引用不变');
  assert.deepEqual(seqs(appendMirrorMessages([], [msg(0), msg(1)])), [0, 1]);

  const prepended = prependOlderMirrorMessages(existing, [msg(9), msg(7), msg(8), msg(10)]);
  assert.deepEqual(seqs(prepended), [7, 8, 9, 10, 11, 12, 13, 14]);
  assert.equal(prependOlderMirrorMessages(existing, [msg(12)]), existing);
});

// ---------------------------------------------------------------------------
// commandSlot
// ---------------------------------------------------------------------------

test('指令单槽:回执落定 ok / failed,点选作答成功推进作答进度', { skip }, async () => {
  const slot = await load('relay/commandSlot.ts');
  const sending = slot.beginCommand(slot.EMPTY_COMMAND_SLOT, 'c1');
  assert.equal(sending.pendingCommandId, 'c1');
  assert.equal(sending.receipt, null);

  const ok = slot.settleCommandReceipt(sending, { commandId: 'c1', ok: true });
  assert.equal(ok.pendingCommandId, null);
  assert.deepEqual(ok.receipt, { commandId: 'c1', status: 'ok' });

  const failed = slot.settleCommandReceipt(sending, {
    commandId: 'c1',
    ok: false,
    reason: 'paneNotFound',
  });
  assert.deepEqual(failed.receipt, { commandId: 'c1', status: 'failed', reason: 'paneNotFound' });

  const answering = slot.beginCommand(slot.EMPTY_COMMAND_SLOT, 'a1', 7);
  assert.deepEqual(answering.pendingAnswer, { commandId: 'a1', seq: 7 });
  const answered = slot.settleCommandReceipt(answering, { commandId: 'a1', ok: true });
  assert.deepEqual(answered.answeredProgress, { 7: 1 });
  assert.equal(answered.pendingAnswer, null);
  const rejected = slot.settleCommandReceipt(answering, {
    commandId: 'a1',
    ok: false,
    reason: 'questionNotPending',
  });
  assert.deepEqual(rejected.answeredProgress, {}, '失败不推进');

  assert.equal(slot.settleCommandReceipt(sending, { commandId: 'other', ok: true }), null);
  assert.equal(slot.settleCommandReceipt(slot.EMPTY_COMMAND_SLOT, { commandId: 'c1', ok: true }), null);
});

test('指令单槽:超时 / 断线 / 桌面掉线 → 结果未知并腾出单槽(不自动重发)', { skip }, async () => {
  const slot = await load('relay/commandSlot.ts');
  const sending = slot.beginCommand(slot.EMPTY_COMMAND_SLOT, 'c1');
  for (const cause of ['timeout', 'disconnected', 'desktopOffline']) {
    const out = slot.expireCommand(sending, cause);
    assert.equal(out.pendingCommandId, null, `${cause}: 单槽腾出`);
    assert.deepEqual(out.receipt, { commandId: 'c1', status: 'unknown', cause });
    assert.deepEqual(out.unsettled, { commandId: 'c1', answerSeq: null });
  }
  assert.equal(slot.expireCommand(sending, 'timeout', 'stale-id'), null, '超时只过期自己那条');
  assert.equal(slot.expireCommand(slot.EMPTY_COMMAND_SLOT, 'disconnected'), null, '没有在途指令');
});

test('指令单槽:结果未知后迟到的回执升级结果;发下一条后旧回执不再认', { skip }, async () => {
  const slot = await load('relay/commandSlot.ts');
  const unknown = slot.expireCommand(slot.beginCommand(slot.EMPTY_COMMAND_SLOT, 'a1', 3), 'timeout');
  assert.deepEqual(unknown.unsettled, { commandId: 'a1', answerSeq: 3 });
  const late = slot.settleCommandReceipt(unknown, { commandId: 'a1', ok: true });
  assert.deepEqual(late.receipt, { commandId: 'a1', status: 'ok' });
  assert.equal(late.unsettled, null);
  assert.deepEqual(late.answeredProgress, { 3: 1 }, '迟到的作答成功同样推进进度');

  const next = slot.beginCommand(unknown, 'c2');
  assert.equal(next.unsettled, null);
  assert.equal(slot.settleCommandReceipt(next, { commandId: 'a1', ok: true }), null);
});

test('指令单槽:换新连接时在途的按断线过期、迟到认领作废', { skip }, async () => {
  const slot = await load('relay/commandSlot.ts');
  const sending = slot.beginCommand(slot.EMPTY_COMMAND_SLOT, 'c1');
  const reset = slot.resetCommandSlotForNewLink(sending);
  assert.equal(reset.pendingCommandId, null);
  assert.equal(reset.unsettled, null);
  assert.deepEqual(reset.receipt, { commandId: 'c1', status: 'unknown', cause: 'disconnected' });

  const unknown = slot.expireCommand(sending, 'disconnected');
  const cleared = slot.resetCommandSlotForNewLink(unknown);
  assert.equal(cleared.unsettled, null);
  assert.deepEqual(cleared.receipt, unknown.receipt, '已展示的「结果未知」提示保留');
  assert.equal(slot.resetCommandSlotForNewLink(slot.EMPTY_COMMAND_SLOT), null);
});

test('指令草稿:只有确定写入成功且用户没改过才清空', { skip }, async () => {
  const { draftAfterReceipt } = await load('relay/commandSlot.ts');
  assert.equal(draftAfterReceipt('ls -la', 'ls -la', 'ok'), '');
  assert.equal(draftAfterReceipt('ls -la && pwd', 'ls -la', 'ok'), 'ls -la && pwd');
  assert.equal(draftAfterReceipt('ls -la', 'ls -la', 'failed'), 'ls -la');
  assert.equal(draftAfterReceipt('ls -la', 'ls -la', 'unknown'), 'ls -la');
});

// ---------------------------------------------------------------------------
// navStack
// ---------------------------------------------------------------------------

test('返回栈:state 标记读写', { skip }, async () => {
  const nav = await load('navStack.ts');
  assert.equal(nav.readNavDepth(null), null);
  assert.equal(nav.readNavDepth({ foo: 1 }), null);
  assert.equal(nav.readNavDepth({ __mtNav: 1, depth: -1 }), null);
  assert.equal(nav.readNavDepth({ __mtNav: 1, depth: 1.5 }), null);
  assert.equal(nav.readNavDepth(nav.makeNavState(3)), 3);
});

test('返回栈:同步规划(push / go / 等自己的 go 落地)', { skip }, async () => {
  const { planSync } = await load('navStack.ts');
  assert.deepEqual(
    planSync({ logicalDepth: 2, browserDepth: 0, pendingGo: 0, currentEntryDepth: null }),
    { kind: 'push', markCurrent: 0, depths: [1, 2] },
    '当前记录没标记:先标成当前深度再 push',
  );
  assert.deepEqual(
    planSync({ logicalDepth: 1, browserDepth: 0, pendingGo: 0, currentEntryDepth: 0 }),
    { kind: 'push', markCurrent: null, depths: [1] },
  );
  assert.deepEqual(
    planSync({ logicalDepth: 0, browserDepth: 3, pendingGo: 0, currentEntryDepth: 3 }),
    { kind: 'go', delta: -3 },
  );
  assert.deepEqual(
    planSync({ logicalDepth: 0, browserDepth: 3, pendingGo: 1, currentEntryDepth: 3 }),
    { kind: 'none' },
  );
  assert.deepEqual(
    planSync({ logicalDepth: 2, browserDepth: 2, pendingGo: 0, currentEntryDepth: 2 }),
    { kind: 'none' },
  );
});

test('返回栈:popstate 规划(用户返回关层 / 片段跳转不算导航 / 自己的 go / 前进纠正)', { skip }, async () => {
  const { planPop } = await load('navStack.ts');
  assert.deepEqual(planPop({ stateDepth: 1, logicalDepth: 3, pendingGo: 0, goTarget: 0 }), {
    browserDepth: 1,
    mark: null,
    close: 2,
    go: 0,
    ours: false,
  });
  assert.deepEqual(planPop({ stateDepth: null, logicalDepth: 2, pendingGo: 0, goTarget: 0 }), {
    browserDepth: 2,
    mark: 2,
    close: 0,
    go: 0,
    ours: false,
  });
  assert.deepEqual(planPop({ stateDepth: 1, logicalDepth: 1, pendingGo: 1, goTarget: 1 }), {
    browserDepth: 1,
    mark: null,
    close: 0,
    go: 0,
    ours: true,
  });
  assert.deepEqual(planPop({ stateDepth: null, logicalDepth: 0, pendingGo: 1, goTarget: 1 }), {
    browserDepth: 1,
    mark: 1,
    close: 0,
    go: 0,
    ours: true,
  });
  assert.deepEqual(planPop({ stateDepth: 2, logicalDepth: 0, pendingGo: 0, goTarget: 0 }), {
    browserDepth: 2,
    mark: null,
    close: 0,
    go: -2,
    ours: false,
  });
});

/**
 * 模拟浏览器历史栈 + navigation.ts 的驱动逻辑(只用纯规划函数),端到端推演典型操作序列。
 * `logical` 是应用的视图层数;userBack / fragmentJump 等模拟用户动作。
 */
function makeBrowserSim(nav) {
  const sim = {
    entries: [{ state: nav.makeNavState(0) }],
    index: 0,
    browserDepth: 0,
    pendingGo: 0,
    goTarget: 0,
    logical: 0,
    exited: false,
    queue: [],
  };
  const current = () => sim.entries[sim.index];
  function goBy(delta) {
    sim.pendingGo += 1;
    sim.goTarget = sim.browserDepth + delta;
    sim.queue.push(() => {
      sim.index += delta;
      onPop(current().state);
    });
  }
  function sync() {
    const plan = nav.planSync({
      logicalDepth: sim.logical,
      browserDepth: sim.browserDepth,
      pendingGo: sim.pendingGo,
      currentEntryDepth: nav.readNavDepth(current().state),
    });
    if (plan.kind === 'push') {
      if (plan.markCurrent !== null) current().state = nav.makeNavState(plan.markCurrent);
      sim.entries.splice(sim.index + 1);
      for (const d of plan.depths) {
        sim.entries.push({ state: nav.makeNavState(d) });
        sim.index += 1;
        sim.browserDepth = d;
      }
    } else if (plan.kind === 'go') {
      goBy(plan.delta);
    }
  }
  function onPop(state) {
    const plan = nav.planPop({
      stateDepth: nav.readNavDepth(state),
      logicalDepth: sim.logical,
      pendingGo: sim.pendingGo,
      goTarget: sim.goTarget,
    });
    if (plan.ours) sim.pendingGo -= 1;
    sim.browserDepth = plan.browserDepth;
    if (plan.mark !== null) current().state = nav.makeNavState(plan.mark);
    sim.logical = Math.max(0, sim.logical - plan.close);
    if (plan.go !== 0) goBy(plan.go);
    sync();
  }
  function flush() {
    while (sim.queue.length) sim.queue.shift()();
  }
  return Object.assign(sim, {
    /** 应用内打开 / 关闭视图(含页内「‹ 返回」),随后同步 */
    setLogical(depth) {
      sim.logical = depth;
      sync();
      flush();
    },
    userBack() {
      if (sim.index === 0) {
        sim.exited = true;
        return;
      }
      sim.index -= 1;
      onPop(current().state);
      flush();
    },
    userForward() {
      if (sim.index >= sim.entries.length - 1) return;
      sim.index += 1;
      onPop(current().state);
      flush();
    },
    /** 点 Markdown 脚注锚点:浏览器新建一条 state=null 的记录并触发 popstate */
    fragmentJump() {
      sim.entries.splice(sim.index + 1);
      sim.entries.push({ state: null });
      sim.index += 1;
      onPop(null);
      flush();
    },
  });
}

test('返回栈推演:逐级返回,根视图再返回才离开', { skip }, async () => {
  const nav = await load('navStack.ts');
  const sim = makeBrowserSim(nav);
  sim.setLogical(1); // 进镜像
  sim.setLogical(2); // 切终端视图
  assert.equal(sim.entries.length, 3);
  sim.userBack();
  assert.equal(sim.logical, 1, '终端 → 对话');
  sim.userBack();
  assert.equal(sim.logical, 0, '镜像 → 列表');
  assert.equal(sim.exited, false);
  sim.userBack();
  assert.equal(sim.exited, true, '根视图按返回才离开');
});

test('返回栈推演:页内返回按钮会退掉对应的历史记录,不留「空返回」', { skip }, async () => {
  const nav = await load('navStack.ts');
  const sim = makeBrowserSim(nav);
  sim.setLogical(1);
  sim.setLogical(2);
  sim.setLogical(1); // 页内切回对话
  assert.equal(sim.index, 1);
  assert.equal(sim.pendingGo, 0);
  sim.setLogical(0); // 页内「‹ 返回」
  assert.equal(sim.index, 0);
  sim.userBack();
  assert.equal(sim.exited, true, '没有残留的空记录');
  // 一次清空多层(切换桌面端)
  const sim2 = makeBrowserSim(nav);
  sim2.setLogical(3);
  sim2.setLogical(0);
  assert.equal(sim2.index, 0);
  assert.equal(sim2.logical, 0);
});

test('返回栈推演:脚注锚点跳转不关视图,从脚注返回仍停在同一层,再返回才退层', { skip }, async () => {
  const nav = await load('navStack.ts');
  const sim = makeBrowserSim(nav);
  sim.setLogical(1); // 镜像
  sim.fragmentJump();
  assert.equal(sim.logical, 1, '跳脚注不算导航');
  assert.equal(nav.readNavDepth(sim.entries[sim.index].state), 1, '脚注记录被标成当前层');
  sim.userBack();
  assert.equal(sim.logical, 1, '从脚注返回:还在镜像');
  sim.userBack();
  assert.equal(sim.logical, 0, '再返回才回列表');

  // 停在脚注记录上又开了下一级:返回只退那一级
  const sim2 = makeBrowserSim(nav);
  sim2.setLogical(1);
  sim2.fragmentJump();
  sim2.setLogical(2);
  sim2.userBack();
  assert.equal(sim2.logical, 1);
  // 停在脚注记录上点页内返回:连同脚注那条记录一起退回到根记录
  sim2.setLogical(0);
  assert.equal(sim2.logical, 0);
  assert.equal(sim2.pendingGo, 0);
  assert.equal(nav.readNavDepth(sim2.entries[sim2.index].state), 0);
});

test('返回栈推演:前进到已关掉的视图会被退回,不复活视图', { skip }, async () => {
  const nav = await load('navStack.ts');
  const sim = makeBrowserSim(nav);
  sim.setLogical(2);
  sim.userBack();
  sim.userBack();
  assert.equal(sim.logical, 0);
  sim.userForward();
  assert.equal(sim.logical, 0);
  assert.equal(sim.index, 0, '自动退回到逻辑深度对应的记录');
});

// ---------------------------------------------------------------------------
// pullGesture
// ---------------------------------------------------------------------------

test('下拉刷新:判顶(含 iOS 回弹负值)', { skip }, async () => {
  const { isAtScrollTop } = await load('pullGesture.ts');
  assert.equal(isAtScrollTop(0), true);
  assert.equal(isAtScrollTop(-12), true, 'iOS 回弹时 scrollTop 为负');
  assert.equal(isAtScrollTop(0.5), true, '亚像素');
  assert.equal(isAtScrollTop(1), false);
  assert.equal(isAtScrollTop(480), false, '列表中段');
});

test('下拉刷新:方向判定(横滑 / 上滑放弃,纵向向下才接管)', { skip }, async () => {
  const { classifyPullGesture, AXIS_LOCK_SLOP } = await load('pullGesture.ts');
  assert.equal(classifyPullGesture(3, 4), 'pending', '位移太小先不判');
  assert.equal(classifyPullGesture(0, AXIS_LOCK_SLOP), 'pull');
  assert.equal(classifyPullGesture(4, 30), 'pull');
  assert.equal(classifyPullGesture(10, 10), 'pull', '45° 不算横向占优');
  assert.equal(classifyPullGesture(30, 10), 'abandon', '横滑');
  assert.equal(classifyPullGesture(-30, 12), 'abandon', '反向横滑');
  assert.equal(classifyPullGesture(2, -20), 'abandon', '往上滚列表');
});

test('下拉刷新:阻尼与上限', { skip }, async () => {
  const { pullDistance, PULL_MAX, PULL_TRIGGER } = await load('pullGesture.ts');
  assert.equal(pullDistance(-10), 0);
  assert.equal(pullDistance(0), 0);
  assert.equal(pullDistance(100), 50);
  assert.equal(pullDistance(10_000), PULL_MAX);
  assert.ok(pullDistance(PULL_TRIGGER * 2) >= PULL_TRIGGER, '拉到两倍触发距离即可触发');
});
