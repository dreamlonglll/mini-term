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
      // date input 的 min 只标 :invalid 不拦截键入,这里兜底 clamp 到 1 年内,
      // 防止久远起始日触发全历史扫描(设计上已移除 'all' 范围)
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
  const day = from && from.getTime() > to.getTime() ? from : to;
  return new Date(day.getFullYear(), day.getMonth(), day.getDate() + 1).getTime() - 1;
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

/** 参数键 → 最近一次 done 整包快照（模块级，跨开关 Modal / 切参数保留）。
 * 只作秒出的旧快照，每次仍会静默重扫校正——不是数据源，无需失效策略 */
const statsCache = new Map<string, UsageStatsPayload>();

/**
 * 统计数据流 hook：拉价 → start_usage_stats → 订阅三事件流式充实。
 * requestId 与后端代际取消双保险；关 Modal（open=false）即 cancel 停扫描。
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
    // 静默刷新：只要手上有数据（同参数重扫保留旧数据；切参数命中快照缓存）就
    // 保持展示，partial 不覆盖，新整包到达后替换；无缓存的新参数才清空回骨架
    const customKey = range === 'custom' ? `${customFrom}:${customTo}` : '';
    const params = `${agents}|${range}|${projectPath ?? ''}|${customKey}`;
    const paramsChanged = lastParamsRef.current !== params;
    lastParamsRef.current = params;
    if (paramsChanged) {
      applyStats(statsCache.get(params) ?? null);
      setProcessed(0);
      setTotal(0);
    }
    silentRef.current = statsRef.current !== null;
    setPhase('pricing');
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
          sinceMs: rangeSinceMs(range, customFrom),
          untilMs: rangeUntilMs(range, customFrom, customTo),
          projectPath,
          tzOffsetMinutes: new Date().getTimezoneOffset(),
          // IANA 时区名:后端按每条记录自身时刻求偏移,DST 地区历史不错日;
          // 解析失败时后端回落上面的固定偏移
          tzName: Intl.DateTimeFormat().resolvedOptions().timeZone,
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
  }, [open, agents, range, projectPath, customFrom, customTo, refreshTick]);

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
      statsCache.set(lastParamsRef.current, p.stats);
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
