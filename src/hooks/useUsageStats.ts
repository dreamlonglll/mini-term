import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useTauriEvent } from './useTauriEvent';
import { loadModelPricing } from '../utils/modelPricing';
import type {
  UsageAgentFilter,
  UsageRange,
  UsageStatsDonePayload,
  UsageStatsErrorPayload,
  UsageStatsPayload,
  UsageStatsProgressPayload,
} from '../types';

/** 渲染状态优先级（互斥）：pricingError ＞ pricing ＞ error ＞ scanning ＞ done */
export type UsageStatsPhase = 'idle' | 'pricing' | 'pricingError' | 'scanning' | 'error' | 'done';

/** range → 窗口起点 epoch ms。本地日历日口径：today = 本地 00:00 起（绝不用
 * 滚动 24h）；days7/30 = 含今天的完整日历日；all = 0。Date 构造器做日历
 * 减法天然处理 DST/月末越界。 */
function rangeSinceMs(range: UsageRange): number {
  if (range === 'all') return 0;
  const now = new Date();
  const daysBack = range === 'today' ? 0 : range === 'days7' ? 6 : 29;
  return new Date(now.getFullYear(), now.getMonth(), now.getDate() - daysBack).getTime();
}

interface UseUsageStatsResult {
  phase: UsageStatsPhase;
  /** 扫描中为 partial 快照，done 后为最终结果 */
  stats: UsageStatsPayload | null;
  processed: number;
  total: number;
  error: string;
  refresh: () => void;
}

/**
 * 统计数据流 hook：拉价 → start_usage_stats → 订阅三事件流式充实。
 * requestId 与后端代际取消双保险；关 Modal（open=false）即 cancel 停扫描。
 */
export function useUsageStats(
  open: boolean,
  agents: UsageAgentFilter,
  range: UsageRange,
): UseUsageStatsResult {
  const [phase, setPhase] = useState<UsageStatsPhase>('idle');
  const [stats, setStats] = useState<UsageStatsPayload | null>(null);
  const [processed, setProcessed] = useState(0);
  const [total, setTotal] = useState(0);
  const [error, setError] = useState('');
  const [refreshTick, setRefreshTick] = useState(0);
  const requestIdRef = useRef('');
  const lastParamsRef = useRef('');
  const statsRef = useRef<UsageStatsPayload | null>(null);
  /** 静默刷新轮次：partial 不覆盖已展示的完整数据，只收 done 的整包替换 */
  const silentRef = useRef(false);

  const applyStats = useCallback((s: UsageStatsPayload | null) => {
    statsRef.current = s;
    setStats(s);
  }, []);

  useEffect(() => {
    if (!open) {
      requestIdRef.current = '';
      lastParamsRef.current = '';
      setPhase('idle');
      return;
    }
    let cancelled = false;
    // 静默刷新：同参数重扫（自动/手动刷新）保留旧数据继续展示，新快照到达后
    // 整包替换；切 scope/range 时旧数据已是错的，必须清空回骨架
    const params = `${agents}|${range}`;
    const paramsChanged = lastParamsRef.current !== params;
    lastParamsRef.current = params;
    silentRef.current = !paramsChanged && statsRef.current !== null;
    setPhase('pricing');
    if (paramsChanged) {
      applyStats(null);
      setProcessed(0);
      setTotal(0);
    }
    setError('');

    (async () => {
      let pricing;
      try {
        pricing = await loadModelPricing();
      } catch (e) {
        if (!cancelled) {
          setPhase('pricingError');
          setError(String(e));
        }
        return;
      }
      if (cancelled) return;

      const requestId = crypto.randomUUID();
      requestIdRef.current = requestId;
      setPhase('scanning');
      try {
        await invoke('start_usage_stats', {
          requestId,
          agents,
          sinceMs: rangeSinceMs(range),
          tzOffsetMinutes: new Date().getTimezoneOffset(),
          hourly: range === 'today',
          pricing,
        });
      } catch (e) {
        if (!cancelled) {
          setPhase('error');
          setError(String(e));
        }
      }
    })();

    return () => {
      cancelled = true;
      requestIdRef.current = '';
      invoke('cancel_usage_stats').catch(() => {});
    };
  }, [open, agents, range, refreshTick]);

  useTauriEvent<UsageStatsProgressPayload>(
    'usage-stats-progress',
    useCallback((p) => {
      if (p.requestId !== requestIdRef.current) return;
      if (!silentRef.current) applyStats(p.partial);
      setProcessed(p.processed);
      setTotal(p.total);
    }, [applyStats]),
  );

  useTauriEvent<UsageStatsDonePayload>(
    'usage-stats-done',
    useCallback((p) => {
      if (p.requestId !== requestIdRef.current) return;
      applyStats(p.stats);
      setPhase('done');
    }, [applyStats]),
  );

  useTauriEvent<UsageStatsErrorPayload>(
    'usage-stats-error',
    useCallback((p) => {
      if (p.requestId !== requestIdRef.current) return;
      setError(p.error);
      setPhase('error');
    }, []),
  );

  const refresh = useCallback(() => setRefreshTick((t) => t + 1), []);

  return { phase, stats, processed, total, error, refresh };
}
