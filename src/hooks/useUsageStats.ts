import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTauriEvent } from './useTauriEvent';
import { loadModelPricing } from '../utils/modelPricing';
import type {
  ModelPriceEntry,
  UsageAgentFilter,
  UsageLedgerProgressPayload,
  UsageLedgerSyncedPayload,
  UsageRange,
  UsageStatsPayload,
} from '../types';

/** 渲染状态优先级（互斥）：pricingError ＞ pricing ＞ error ＞ ready */
export type UsageStatsPhase = 'pricing' | 'pricingError' | 'ready' | 'error';

/** date input 的 "YYYY-MM-DD" 按本地时区解析(new Date(str) 会当 UTC 午夜,东侧时区错一天)。 */
function parseLocalDate(s: string): Date | null {
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(s);
  if (!m) return null;
  return new Date(Number(m[1]), Number(m[2]) - 1, Number(m[3]));
}

/** range → 窗口起点 epoch ms。本地日历日口径：today = 本地 00:00 起（绝不用
 * 滚动 24h）；days7/30 = 含今天的完整日历日；month/months3/months6 = 对应
 * 月份的月初；custom = 起始日本地 00:00。Date 构造器做日历减法天然处理
 * DST/月末越界。 */
function rangeSinceMs(range: UsageRange, customFrom: string): number {
  const now = new Date();
  switch (range) {
    case 'today':
      return new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    case 'days7':
      return new Date(now.getFullYear(), now.getMonth(), now.getDate() - 6).getTime();
    case 'days30':
      return new Date(now.getFullYear(), now.getMonth(), now.getDate() - 29).getTime();
    case 'month':
      return new Date(now.getFullYear(), now.getMonth(), 1).getTime();
    case 'months3':
      return new Date(now.getFullYear(), now.getMonth() - 2, 1).getTime();
    case 'months6':
      return new Date(now.getFullYear(), now.getMonth() - 5, 1).getTime();
    case 'custom': {
      const from = parseLocalDate(customFrom);
      // 起始缺失/非法回落近 30 天,不让面板空转
      if (!from) return new Date(now.getFullYear(), now.getMonth(), now.getDate() - 29).getTime();
      // date input 的 min 只标 :invalid 不拦截键入,这里兜底 clamp 到 1 年内
      const floor = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 364).getTime();
      return Math.max(from.getTime(), floor);
    }
  }
}

/** custom range 的窗口上界(含截止日全天);其余 range 开区间到现在。 */
function rangeUntilMs(range: UsageRange, customFrom: string, customTo: string): number | null {
  if (range !== 'custom') return null;
  const to = parseLocalDate(customTo);
  if (!to) return null;
  // date input 的 min 只标 :invalid 不拦截键入,键盘可造出 from>to 的倒置区间;
  // 倒置时把上界抬到起始日(等效单日查询),避免静默全零
  const from = parseLocalDate(customFrom);
  let day = from && from.getTime() > to.getTime() ? from : to;
  // 与 rangeSinceMs 的一年下限同步 clamp:两端都早于一年时退成下限当日的
  // 单日窗口,而不是 since 被抬、until 不动产生的 since>until 倒置空窗
  const now = new Date();
  const floor = new Date(now.getFullYear(), now.getMonth(), now.getDate() - 364);
  if (day.getTime() < floor.getTime()) day = floor;
  return new Date(day.getFullYear(), day.getMonth(), day.getDate() + 1).getTime() - 1;
}

interface UseUsageStatsResult {
  phase: UsageStatsPhase;
  stats: UsageStatsPayload | null;
  /** backfill（账本首建全量同步）进度；非 backfill 期间恒 0/0 */
  backfillProcessed: number;
  backfillTotal: number;
  error: string;
  /** 手动刷新：重拉价（若失败过）→ 重查 → 触发增量同步 */
  refresh: () => void;
  /** 仅触发一次增量同步（自动刷新定时器用；数据有变由 synced 事件驱动重查） */
  sync: () => void;
}

/**
 * 统计数据流 hook（账本化）：展示只查账本（usage_ledger_query 毫秒级秒出），
 * 增量同步在后台跑（usage_ledger_sync），synced 事件驱动重查。
 * 切参数就是重新查询——无扫描态、无快照缓存、无静默机制。
 */
export function useUsageStats(
  open: boolean,
  agents: UsageAgentFilter,
  range: UsageRange,
  /** 单项目 scope:登记项目绝对路径;null = 整机全部 */
  projectPath: string | null,
  /** custom range 起止("YYYY-MM-DD");其余 range 忽略 */
  customFrom: string,
  customTo: string,
): UseUsageStatsResult {
  const [phase, setPhase] = useState<UsageStatsPhase>('pricing');
  const [stats, setStats] = useState<UsageStatsPayload | null>(null);
  const [backfill, setBackfill] = useState({ processed: 0, total: 0 });
  const [error, setError] = useState('');
  const [refreshTick, setRefreshTick] = useState(0);
  /** 价格表跨开关 Modal 保留（loadModelPricing 另有 24h localStorage 缓存） */
  const pricingRef = useRef<Record<string, ModelPriceEntry> | null>(null);
  /** 查询竞态防护：只采纳最新一次查询的结果 */
  const seqRef = useRef(0);
  const openRef = useRef(open);
  openRef.current = open;
  /** stats/phase 的权威镜像：refresh effect 里判断状态用（不进依赖数组） */
  const statsRef = useRef<UsageStatsPayload | null>(null);
  const phaseRef = useRef<UsageStatsPhase>('pricing');
  phaseRef.current = phase;

  const query = useCallback(async () => {
    if (!pricingRef.current || !openRef.current) return;
    const seq = ++seqRef.current;
    try {
      const s = await invoke<UsageStatsPayload>('usage_ledger_query', {
        agents,
        sinceMs: rangeSinceMs(range, customFrom),
        untilMs: rangeUntilMs(range, customFrom, customTo),
        projectPath,
        tzOffsetMinutes: new Date().getTimezoneOffset(),
        // IANA 时区名:后端按每条记录自身时刻求偏移,DST 地区历史不错日;
        // 解析失败时后端回落上面的固定偏移
        tzName: Intl.DateTimeFormat().resolvedOptions().timeZone,
        hourly: range === 'today',
        pricing: pricingRef.current,
      });
      if (seq === seqRef.current) {
        // 内容未变就保留旧引用：整包替换会让整棵子树重渲染,recharts 会把
        // 新引用当「数据变了」重启动画(连击下图形卡在起始帧,表现为消失)
        const prev = statsRef.current;
        const next = prev && JSON.stringify(prev) === JSON.stringify(s) ? prev : s;
        statsRef.current = next;
        setStats(next);
        setPhase('ready');
      }
    } catch (e) {
      if (seq === seqRef.current) {
        setError(String(e));
        setPhase('error');
      }
    }
  }, [agents, range, projectPath, customFrom, customTo]);
  const queryRef = useRef(query);
  queryRef.current = query;

  // 打开面板 / 手动刷新：拉价（缓存命中即瞬时）→ 后台增量同步。
  // 已就绪(ready 且有数据)时刷新**只发 sync**——synced(added>0) 驱动重查,
  // 与自动刷新完全同路径,账本没变时零请求零渲染;
  // 仅拉价补查与错误恢复需要主动 query(sync added=0 时不会驱动查询)
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    (async () => {
      if (!pricingRef.current) {
        setPhase('pricing');
        setError('');
        try {
          pricingRef.current = await loadModelPricing();
        } catch (e) {
          if (!cancelled) {
            setError(String(e));
            setPhase('pricingError');
          }
          return;
        }
        if (cancelled) return;
        queryRef.current();
      } else if (statsRef.current === null || phaseRef.current === 'error') {
        queryRef.current();
      }
      invoke('usage_ledger_sync').catch(() => {});
    })();
    return () => {
      cancelled = true;
    };
  }, [open, refreshTick]);

  // 切参数 → 直接重新查询（毫秒级；价格未就绪时由上面的 effect 拉完价补查）
  useEffect(() => {
    if (!open) return;
    query();
  }, [open, query]);

  useTauriEvent<UsageLedgerProgressPayload>('usage-ledger-progress', (p) => {
    if (openRef.current) setBackfill({ processed: p.processed, total: p.total });
  });

  useTauriEvent<UsageLedgerSyncedPayload>('usage-ledger-synced', (p) => {
    // 值未变时保留旧引用:每 5s 的空转 sync(added=0)不得触发重渲染
    // (Modal 重渲染本身就会被 recharts 感知,见 DailyChart 的 buckets memo)
    setBackfill((prev) =>
      prev.processed === 0 && prev.total === 0 ? prev : { processed: 0, total: 0 },
    );
    // added = 0 表示账本无变化,跳过重查避免无谓重渲染
    if (openRef.current && p.added > 0) queryRef.current();
  });

  const refresh = useCallback(() => setRefreshTick((t) => t + 1), []);
  const sync = useCallback(() => {
    invoke('usage_ledger_sync').catch(() => {});
  }, []);

  return {
    phase,
    stats,
    backfillProcessed: backfill.processed,
    backfillTotal: backfill.total,
    error,
    refresh,
    sync,
  };
}
