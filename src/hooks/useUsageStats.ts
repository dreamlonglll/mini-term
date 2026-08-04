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
  /** 手动刷新正在等后端同步跑完（刷新按钮据此显示忙态、避免重复点击） */
  syncing: boolean;
  error: string;
  /** 手动刷新：重拉价（24h TTL 缓存命中即瞬时）→ 等增量同步跑完 → 再查 */
  refresh: () => void;
  /** 仅触发一次增量同步（自动刷新定时器用；数据有变由 synced 事件驱动重查） */
  sync: () => void;
}

/**
 * 统计数据流 hook（账本化）：展示只查账本（usage_ledger_query 毫秒级秒出）。
 * 切参数就是重新查询——无扫描态、无快照缓存、无静默机制。
 *
 * 同步有两条路径，差别在「查询与同步的先后」：
 * - 打开面板 / 定时自动刷新：先出账本现值，同步在后台跑，有变化再由
 *   `usage-ledger-synced` 事件驱动补查（面板不空屏、定时器不阻塞）。
 * - 手动刷新：先等同步跑完（`wait: true`）再查，数字一步到位。否则点一次
 *   会先看到同步前的旧值、真值随后才补上，活跃使用或首次全量回填时表现为
 *   金额大幅跳动。
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
  const [syncing, setSyncing] = useState(false);
  const [error, setError] = useState('');
  const [refreshTick, setRefreshTick] = useState(0);
  /** 已消费的刷新序号：用来把「打开面板」与「点刷新」两条路径分开
   *  （关闭再打开时 tick 不变，不该退化成一次阻塞刷新） */
  const handledTickRef = useRef(0);
  /** 价格表跨开关 Modal 保留；每次打开/刷新都重走 loadModelPricing，24h TTL 由其缓存判定 */
  const pricingRef = useRef<Record<string, ModelPriceEntry> | null>(null);
  /** 查询竞态防护：只采纳最新一次查询的结果 */
  const seqRef = useRef(0);
  /** backfill 进度驱动重查的节流时钟 */
  const lastProgressQueryRef = useRef(0);
  const openRef = useRef(open);
  openRef.current = open;
  /** stats 的权威镜像：query 里做内容比对用（不进依赖数组） */
  const statsRef = useRef<UsageStatsPayload | null>(null);

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

  // 拉价：24h TTL 由 loadModelPricing 的 localStorage 缓存判定（命中即瞬时），
  // 内存引用不再永久绕过 TTL——应用常驻多日后过期价格照常重拉；
  // 已有旧价格表时拉新失败静默沿用（不把可用面板打成错误态）。
  // 返回是否可以继续查询。
  const ensurePricing = useCallback(async (): Promise<boolean> => {
    if (!pricingRef.current) {
      setPhase('pricing');
      setError('');
    }
    try {
      pricingRef.current = await loadModelPricing();
      return true;
    } catch (e) {
      if (pricingRef.current) return true;
      setError(String(e));
      setPhase('pricingError');
      return false;
    }
  }, []);

  // 打开面板：先出账本现值（查询毫秒级，不让面板空屏干等），再后台增量同步；
  // 同步有变由 synced 事件驱动补查
  useEffect(() => {
    if (!open) return;
    let cancelled = false;
    (async () => {
      const ok = await ensurePricing();
      if (cancelled || !ok) return;
      queryRef.current();
      invoke('usage_ledger_sync', { wait: false }).catch(() => {});
    })();
    return () => {
      cancelled = true;
    };
  }, [open, ensurePricing]);

  // 手动刷新：等增量同步真正跑完再查 —— 数字一步到位。
  // 旧实现是「先查、再触发同步」，每点一次必然先闪一次同步前的旧值，真值要等
  // synced 事件才补上；在活跃使用或首次全量回填时表现为金额大幅跳动。
  useEffect(() => {
    if (!open || refreshTick === handledTickRef.current) return;
    handledTickRef.current = refreshTick;
    let cancelled = false;
    (async () => {
      const ok = await ensurePricing();
      if (cancelled || !ok) return;
      setSyncing(true);
      try {
        await invoke('usage_ledger_sync', { wait: true });
      } catch {
        // 同步失败仍查一次账本现值，不把可用面板打成错误态
      }
      if (cancelled) return;
      setSyncing(false);
      queryRef.current();
    })();
    return () => {
      cancelled = true;
      setSyncing(false);
    };
  }, [open, refreshTick, ensurePricing]);

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
  // 定时自动刷新：非阻塞触发，不占用调用方；数据有变由 synced 事件驱动重查
  const sync = useCallback(() => {
    invoke('usage_ledger_sync', { wait: false }).catch(() => {});
  }, []);

  return {
    phase,
    stats,
    backfillProcessed: backfill.processed,
    backfillTotal: backfill.total,
    syncing,
    error,
    refresh,
    sync,
  };
}
