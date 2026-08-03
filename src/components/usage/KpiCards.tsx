import { useT } from '../../i18n';
import type { UsageStatsPayload } from '../../types';
import { cacheHitRate, formatCost, formatCount } from './format';
import { useTweenedNumber } from './useTween';

// === 图标（统一 16 viewBox / stroke currentColor，对齐 ActivityBar 画风）===
const ICON_WALLET = (
  <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <rect x="2" y="4" width="12" height="9" rx="1.5" />
    <path d="M2 6.5h12M10.5 9.5h1.5" />
  </svg>
);
const ICON_PULSE = (
  <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M2 8h3l1.5-4 3 8L11 8h3" />
  </svg>
);
const ICON_CHAT = (
  <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M8 2.5a5.5 5.5 0 1 0-4.9 8L2.5 13.5l3.1-.6A5.5 5.5 0 1 0 8 2.5z" />
  </svg>
);
const ICON_BOLT = (
  <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M8.5 2 3.5 9h3.5l-.5 5 5-7H8l.5-5z" />
  </svg>
);

interface KpiCardProps {
  icon: React.ReactNode;
  iconColor: string;
  label: string;
  value: string;
  valueColor?: string;
}

function KpiCard({ icon, iconColor, label, value, valueColor }: KpiCardProps) {
  return (
    <div className="flex items-center gap-3 px-4 py-3.5 bg-[var(--bg-elevated)] border border-[var(--border-subtle)] rounded-[var(--radius-md)]">
      <span
        className="w-8 h-8 flex items-center justify-center rounded-[var(--radius-sm)] flex-shrink-0"
        style={{ color: iconColor, backgroundColor: 'color-mix(in srgb, currentColor 12%, transparent)' }}
      >
        {icon}
      </span>
      <div className="min-w-0">
        <div className="text-[10px] font-semibold tracking-wider uppercase text-[var(--text-muted)]">{label}</div>
        <div
          className="text-xl font-bold leading-tight truncate tabular-nums"
          style={{ color: valueColor ?? 'var(--text-primary)' }}
        >
          {value}
        </div>
      </div>
    </div>
  );
}

export function KpiCards({ stats }: { stats: UsageStatsPayload }) {
  const t = useT();
  const hit = cacheHitRate(stats.inputTokens, stats.cacheReadTokens, stats.cacheWriteTokens);
  // 数字滚动:数据更新时从旧值补间到新值(等宽字体下无位移)
  const cost = useTweenedNumber(stats.totalCost);
  const callsN = useTweenedNumber(stats.totalCalls);
  const sessionsN = useTweenedNumber(stats.sessionCount);
  const hitN = useTweenedNumber(hit ?? 0);
  return (
    <div className="grid grid-cols-4 gap-3">
      <KpiCard
        icon={ICON_WALLET}
        iconColor="var(--color-info)"
        label={t('usageStats.kpi.cost')}
        value={formatCost(cost)}
        valueColor="var(--accent)"
      />
      <KpiCard
        icon={ICON_PULSE}
        iconColor="var(--color-info)"
        label={t('usageStats.kpi.calls')}
        value={formatCount(Math.round(callsN))}
      />
      <KpiCard
        icon={ICON_CHAT}
        iconColor="var(--color-info)"
        label={t('usageStats.kpi.sessions')}
        value={formatCount(Math.round(sessionsN))}
      />
      <KpiCard
        icon={ICON_BOLT}
        iconColor="var(--color-info)"
        label={t('usageStats.kpi.cacheHit')}
        value={hit === null ? '—' : `${hitN.toFixed(1)}%`}
      />
    </div>
  );
}
