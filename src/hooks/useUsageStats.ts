import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTauriEvent } from './useTauriEvent';
import { loadModelPricing } from '../utils/modelPricing';
import { rangeSinceMs, rangeUntilMs } from '../utils/usageDates';
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
  /** backfill 进度驱动重查的节流时钟 */
  const lastProgressQueryRef = useRef(0);
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
    if (!openRef.current) return;
    setBackfill({ processed: p.processed, total: p.total });
    // backfill 增量填充:进度事件(后端已 250ms 节流)按 ~1s 再节流触发重查,
    // 图表/KPI 随回填逐步长出,不再干等终局 synced 一次性全出
    // (查询毫秒级且跑在 runtime 线程,代价可忽略)
    const now = Date.now();
    if (now - lastProgressQueryRef.current >= 1000) {
      lastProgressQueryRef.current = now;
      queryRef.current();
    }
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
