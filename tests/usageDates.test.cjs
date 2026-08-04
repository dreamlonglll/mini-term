const assert = require('node:assert/strict');
const test = require('node:test');

const {
  acceptDateInput,
  rangeStartDate,
  rangeSinceMs,
  rangeUntilMs,
} = require('../.tmp-tests/utils/usageDates.js');

// custom 起止日期的提交闸门:只有完整 YYYY-MM-DD 才更新承诺值。
// 空串/非法值一旦进入查询状态,custom 窗口会静默退化为无上界/近30天。

test('完整日期被接受', () => {
  assert.equal(acceptDateInput('2026-08-03', '2026-07-01'), '2026-08-03');
});

test('清空(空串)保持上一个有效值', () => {
  assert.equal(acceptDateInput('', '2026-07-01'), '2026-07-01');
});

test('非法格式保持上一个有效值', () => {
  assert.equal(acceptDateInput('2026-8-3', '2026-07-01'), '2026-07-01');
  assert.equal(acceptDateInput('08/03/2026', '2026-07-01'), '2026-07-01');
  assert.equal(acceptDateInput('garbage', '2026-07-01'), '2026-07-01');
});

// 范围元数据:窗口起点/起止规则集中一处,Modal 清单、查询窗口、图表补桶同源。

const NOW = new Date(2026, 7, 3, 15, 30); // 2026-08-03 本地

test('rangeStartDate 按日历日/月首回溯', () => {
  assert.equal(rangeStartDate('today', NOW).getTime(), new Date(2026, 7, 3).getTime());
  assert.equal(rangeStartDate('days7', NOW).getTime(), new Date(2026, 6, 28).getTime());
  assert.equal(rangeStartDate('days30', NOW).getTime(), new Date(2026, 7, 3 - 29).getTime());
  assert.equal(rangeStartDate('month', NOW).getTime(), new Date(2026, 7, 1).getTime());
  assert.equal(rangeStartDate('months3', NOW).getTime(), new Date(2026, 5, 1).getTime());
  assert.equal(rangeStartDate('months6', NOW).getTime(), new Date(2026, 2, 1).getTime());
  assert.equal(rangeStartDate('custom', NOW), null);
});

test('rangeSinceMs 非 custom 与 rangeStartDate 同源', () => {
  assert.equal(rangeSinceMs('days30', '', NOW), new Date(2026, 7, 3 - 29).getTime());
  assert.equal(rangeSinceMs('months6', '', NOW), new Date(2026, 2, 1).getTime());
});

test('rangeSinceMs custom:合法起点生效,非法回落近30天,过旧钳到一年', () => {
  assert.equal(rangeSinceMs('custom', '2026-08-01', NOW), new Date(2026, 7, 1).getTime());
  assert.equal(rangeSinceMs('custom', '', NOW), new Date(2026, 7, 3 - 29).getTime());
  assert.equal(rangeSinceMs('custom', '2020-01-01', NOW), new Date(2026, 7, 3 - 364).getTime());
});

test('rangeUntilMs:非 custom 开区间;custom 含截止日全天', () => {
  assert.equal(rangeUntilMs('days30', '', '', NOW), null);
  assert.equal(rangeUntilMs('custom', '2026-08-01', '2026-08-02', NOW), new Date(2026, 7, 3).getTime() - 1);
});

test('rangeUntilMs custom:倒置抬到起始日,两端过旧退成一年下限单日', () => {
  // from > to → 上界抬到 from 当日(等效单日)
  assert.equal(rangeUntilMs('custom', '2026-08-02', '2026-08-01', NOW), new Date(2026, 7, 3).getTime() - 1);
  // 两端都早于一年 → 下限当日单日窗口,不产生 since>until 空窗
  const floorEnd = new Date(2026, 7, 3 - 364 + 1).getTime() - 1;
  assert.equal(rangeUntilMs('custom', '2020-01-01', '2020-02-01', NOW), floorEnd);
  const since = rangeSinceMs('custom', '2020-01-01', NOW);
  assert.ok(since <= floorEnd, 'since 不得大于 until');
});

