/** 使用统计的日期与范围规则（纯函数，可进 node --test）。
 *
 * UsageRange 的范围清单、每范围的窗口规格、since/until 计算与图表补桶起点
 * 全部同源于此——新增/修改范围只改本文件（Repeated Switches 收口）。
 */
import type { UsageRange } from '../types';

/** 面板提供的范围清单。设计合同(docs/plans/2026-08-01-usage-stats-design.md §2)：
 *  不提供 all（全盘扫描太重）。 */
export const USAGE_RANGES: readonly UsageRange[] = [
  'today', 'days7', 'days30', 'month', 'months3', 'months6', 'custom',
];

/** 每范围的窗口规格：本地日历日回溯 / 月首回溯。 */
type RangeSpec =
  | { kind: 'today' }
  | { kind: 'days'; back: number }
  | { kind: 'months'; back: number }
  | { kind: 'custom' };

const RANGE_SPECS: Record<UsageRange, RangeSpec> = {
  today: { kind: 'today' },
  days7: { kind: 'days', back: 6 },
  days30: { kind: 'days', back: 29 },
  month: { kind: 'months', back: 0 },
  months3: { kind: 'months', back: 2 },
  months6: { kind: 'months', back: 5 },
  custom: { kind: 'custom' },
};

/** custom 起点的最早允许日（近一年）；date input 的 min 只标 :invalid 不拦截键入。 */
function customFloor(now: Date): Date {
  return new Date(now.getFullYear(), now.getMonth(), now.getDate() - 364);
}

/** 非 custom 范围的窗口起始日（本地日历，Date 构造器做日历减法天然处理
 *  DST/月末越界）；custom 由起止输入决定，返回 null。图表补桶与查询窗口共用。 */
export function rangeStartDate(range: UsageRange, now = new Date()): Date | null {
  const spec = RANGE_SPECS[range];
  switch (spec.kind) {
    case 'today':
      return new Date(now.getFullYear(), now.getMonth(), now.getDate());
    case 'days':
      return new Date(now.getFullYear(), now.getMonth(), now.getDate() - spec.back);
    case 'months':
      return new Date(now.getFullYear(), now.getMonth() - spec.back, 1);
    case 'custom':
      return null;
  }
}

/** date input 的 "YYYY-MM-DD" 按本地时区解析(new Date(str) 会当 UTC 午夜,东侧时区错一天)。 */
export function parseLocalDate(s: string): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(s);
  if (!m) return null;
  return new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]));
}

/** range → 窗口起点 epoch ms。本地日历日口径：today = 本地 00:00 起（绝不用
 * 滚动 24h）；days7/30 = 含今天的完整日历日；month/months3/months6 = 对应
 * 月份的月初；custom = 起始日本地 00:00（缺失/非法回落近 30 天，过旧钳到一年内）。 */
export function rangeSinceMs(range: UsageRange, customFrom: string, now = new Date()): number {
  const start = rangeStartDate(range, now);
  if (start) return start.getTime();
  const from = parseLocalDate(customFrom);
  // 起始缺失/非法回落近 30 天,不让面板空转
  if (!from) return rangeStartDate('days30', now)!.getTime();
  return Math.max(from.getTime(), customFloor(now).getTime());
}

/** custom range 的窗口上界(含截止日全天);其余 range 开区间到现在。 */
export function rangeUntilMs(
  range: UsageRange,
  customFrom: string,
  customTo: string,
  now = new Date(),
): number | null {
  if (range !== 'custom') return null;
  const to = parseLocalDate(customTo);
  if (!to) return null;
  // 键盘可造出 from>to 的倒置区间;倒置时把上界抬到起始日(等效单日查询),
  // 避免静默全零
  const from = parseLocalDate(customFrom);
  let day = from && from.getTime() > to.getTime() ? from : to;
  // 与 rangeSinceMs 的一年下限同步 clamp:两端都早于一年时退成下限当日的
  // 单日窗口,而不是 since 被抬、until 不动产生的 since>until 倒置空窗
  const floor = customFloor(now);
  if (day.getTime() < floor.getTime()) day = floor;
  return new Date(day.getFullYear(), day.getMonth(), day.getDate() + 1).getTime() - 1;
}

/** custom 趋势图的补桶窗口（本地日历日）：与查询窗口同源——起点走 rangeSinceMs
 *  （非法回落近 30 天、过旧钳一年），终点走 rangeUntilMs 的截止日（缺失/非法 =
 *  无上界 → 补到今天；倒置抬到起始日）。图表轴反映所选窗口而非数据跨度。 */
export function customChartWindow(
  customFrom: string,
  customTo: string,
  now = new Date(),
): { start: Date; end: Date } {
  const start = new Date(rangeSinceMs('custom', customFrom, now));
  const until = rangeUntilMs('custom', customFrom, customTo, now);
  const endDay = until === null ? now : new Date(until);
  let end = new Date(endDay.getFullYear(), endDay.getMonth(), endDay.getDate());
  if (end.getTime() < start.getTime()) end = start;
  return { start, end };
}

/** custom 起止日期输入的提交闸门。
 *
 * 原生 `<input type="date">` 的 change 只会给出完整 `YYYY-MM-DD` 或清空的 `''`；
 * 空串/非法值一旦进入查询状态，custom 窗口会静默退化为无上界（To 缺失）或
 * 近 30 天回退（From 缺失），展示远大于所选范围的数据。因此只有完整日期才
 * 更新承诺值，其余输入保持上一个有效值（受控输入随之回弹）。
 */
export function acceptDateInput(next: string, prev: string): string {
  return /^\d{4}-\d{2}-\d{2}$/.test(next) ? next : prev;
}
