import { useLayoutEffect, useRef, useState } from 'react';
import { useT } from '../../i18n';
import type { UsageDailyStat, UsageRange } from '../../types';
import { formatCost, formatCount, formatTokens } from './format';

/** 轴上限取 1/2/2.5/5×10ⁿ 阶梯，让刻度值可读 */
function niceMax(v: number): number {
  if (v <= 0) return 1;
  const base = Math.pow(10, Math.floor(Math.log10(v)));
  const f = v / base;
  const nf = f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10;
  return nf * base;
}

function axisCost(v: number): string {
  return v >= 1000 ? `$${(v / 1000).toFixed(1)}K` : `$${v.toFixed(2)}`;
}

function dayKey(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

/** 补齐空桶：今天视图从 00:00 到当前小时；日粒度补窗口（all 用数据首日）到今天。
 * 后端快照是稀疏的（只有有数据的桶），无活动时段补 0 才能画出图 5 那样的完整时间轴 */
function fillBuckets(daily: UsageDailyStat[], range: UsageRange): UsageDailyStat[] {
  const map = new Map(daily.map((d) => [d.date, d]));
  const out: UsageDailyStat[] = [];
  const empty = (date: string): UsageDailyStat => ({
    date,
    cost: 0,
    calls: 0,
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
  });
  const now = new Date();
  if (range === 'today') {
    for (let h = 0; h <= now.getHours(); h++) {
      const key = `${String(h).padStart(2, '0')}:00`;
      out.push(map.get(key) ?? empty(key));
    }
    return out;
  }
  if (daily.length === 0) return out;
  let start: Date;
  if (range === 'days7' || range === 'days30') {
    const daysBack = range === 'days7' ? 6 : 29;
    start = new Date(now.getFullYear(), now.getMonth(), now.getDate() - daysBack);
  } else {
    // month/months3/months6/custom:窗口起点不在本组件已知,从数据首日起补零
    const [y, m, d] = daily[0].date.split('-').map(Number);
    start = new Date(y, m - 1, d);
  }
  // custom 的截止日可能远在过去,补零到今天只会画一条零尾巴 → 止于数据末日
  const end = (() => {
    if (range === 'custom') {
      const [y, m, d] = daily[daily.length - 1].date.split('-').map(Number);
      return new Date(y, m - 1, d);
    }
    return new Date(now.getFullYear(), now.getMonth(), now.getDate());
  })();
  for (
    let cur = start;
    cur <= end;
    cur = new Date(cur.getFullYear(), cur.getMonth(), cur.getDate() + 1)
  ) {
    const key = dayKey(cur);
    out.push(map.get(key) ?? empty(key));
  }
  return out;
}

const H = 232;
const PAD_L = 52;
const PAD_R = 44;
const PAD_T = 10;
const PAD_B = 20;
const GRID_ROWS = 4;

/**
 * 时段活动图：cost 折线（左轴）+ calls 柱（右轴），纯 SVG 不引图表库。
 * 「今天」按小时分桶，其余按日历日。hover 用原生 <title> 提示；
 * 补齐后仍只有 1 个桶时退化为摘要卡（孤点图没有信息量）。
 */
export function DailyChart({ daily, range }: { daily: UsageDailyStat[]; range: UsageRange }) {
  const t = useT();
  const containerRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);

  useLayoutEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setWidth(el.clientWidth));
    ro.observe(el);
    setWidth(el.clientWidth);
    return () => ro.disconnect();
  }, []);

  if (daily.length === 0) {
    return (
      <div className="h-[232px] flex items-center justify-center text-sm text-[var(--text-muted)]">
        {t('usageStats.noDailyData')}
      </div>
    );
  }

  const buckets = fillBuckets(daily, range);

  // 补齐后仍单桶（如 0 点刚过打开「今天」）：摘要卡
  if (buckets.length === 1) {
    const d = buckets[0];
    return (
      <div className="h-[232px] flex flex-col items-center justify-center gap-1.5">
        <div className="text-xs text-[var(--text-muted)]">{d.date}</div>
        <div className="text-3xl font-bold text-[var(--accent)]">{formatCost(d.cost)}</div>
        <div className="text-sm text-[var(--text-secondary)]">
          {t('usageStats.callsCount', { count: formatCount(d.calls) })}
        </div>
      </div>
    );
  }

  return (
    <div ref={containerRef} className="w-full">
      {width > 0 && <ChartSvg daily={buckets} width={width} />}
    </div>
  );
}

function ChartSvg({ daily, width }: { daily: UsageDailyStat[]; width: number }) {
  const t = useT();
  const [hover, setHover] = useState<number | null>(null);
  const n = daily.length;
  const plotW = Math.max(width - PAD_L - PAD_R, 10);
  const plotH = H - PAD_T - PAD_B;
  const slot = plotW / n;

  const costMax = niceMax(Math.max(...daily.map((d) => d.cost)));
  const callsMax = niceMax(Math.max(...daily.map((d) => d.calls)));

  const yCost = (v: number) => PAD_T + plotH * (1 - v / costMax);
  const yCalls = (v: number) => PAD_T + plotH * (1 - v / callsMax);
  const xMid = (i: number) => PAD_L + slot * (i + 0.5);

  const linePath = daily
    .map((d, i) => `${i === 0 ? 'M' : 'L'}${xMid(i).toFixed(1)},${yCost(d.cost).toFixed(1)}`)
    .join('');
  const baseline = PAD_T + plotH;
  const areaPath = `${linePath}L${xMid(n - 1).toFixed(1)},${baseline}L${xMid(0).toFixed(1)},${baseline}Z`;

  const dotR = n <= 40 ? 2.5 : n <= 90 ? 1.8 : 0;
  const barW = Math.min(Math.max(slot * 0.55, 1), 14);
  const labelStep = Math.ceil(n / 6);
  const hovered = hover !== null ? daily[hover] : null;

  return (
    <div className="relative" onMouseLeave={() => setHover(null)}>
      <svg width={width} height={H} className="block select-none">
        <defs>
          <linearGradient id="usage-daily-area" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="var(--accent)" stopOpacity="0.3" />
            <stop offset="100%" stopColor="var(--accent)" stopOpacity="0.02" />
          </linearGradient>
        </defs>

        {/* 网格 + 双轴刻度 */}
        {Array.from({ length: GRID_ROWS + 1 }, (_, r) => {
          const y = PAD_T + (plotH * r) / GRID_ROWS;
          const costV = costMax * (1 - r / GRID_ROWS);
          const callsV = callsMax * (1 - r / GRID_ROWS);
          return (
            <g key={r}>
              <line
                x1={PAD_L}
                y1={y}
                x2={width - PAD_R}
                y2={y}
                stroke="var(--border-default)"
                strokeDasharray={r === GRID_ROWS ? undefined : '3 4'}
              />
              <text x={PAD_L - 6} y={y + 3} textAnchor="end" fontSize="9" fill="var(--text-muted)">
                {axisCost(costV)}
              </text>
              <text x={width - PAD_R + 6} y={y + 3} textAnchor="start" fontSize="9" fill="var(--text-muted)">
                {formatCount(Math.round(callsV))}
              </text>
            </g>
          );
        })}

        {/* calls 柱（右轴） */}
        {daily.map((d, i) => (
          <rect
            key={`b${i}`}
            x={xMid(i) - barW / 2}
            y={yCalls(d.calls)}
            width={barW}
            height={Math.max(baseline - yCalls(d.calls), 0)}
            rx={Math.min(2, barW / 2)}
            fill="var(--text-muted)"
            opacity={hover === i ? 0.5 : 0.28}
          />
        ))}

        {/* cost 面积 + 折线 + 数据点（左轴） */}
        <path d={areaPath} fill="url(#usage-daily-area)" />
        <path d={linePath} fill="none" stroke="var(--accent)" strokeWidth="1.8" strokeLinejoin="round" />
        {dotR > 0 &&
          daily.map((d, i) => (
            <circle key={`p${i}`} cx={xMid(i)} cy={yCost(d.cost)} r={dotR} fill="var(--accent)" />
          ))}

        {/* hover 参考线 + 高亮点 */}
        {hover !== null && (
          <g pointerEvents="none">
            <line
              x1={xMid(hover)}
              y1={PAD_T}
              x2={xMid(hover)}
              y2={baseline}
              stroke="var(--text-muted)"
              strokeDasharray="3 3"
            />
            <circle
              cx={xMid(hover)}
              cy={yCost(daily[hover].cost)}
              r={4}
              fill="var(--accent)"
              stroke="var(--bg-surface)"
              strokeWidth="1.5"
            />
          </g>
        )}

        {/* X 轴稀疏标签（小时桶 "HH:00" 原样，日桶截成 MM-DD） */}
        {daily.map((d, i) =>
          i % labelStep === 0 || i === n - 1 ? (
            <text
              key={`x${i}`}
              x={xMid(i)}
              y={H - 6}
              textAnchor="middle"
              fontSize="9"
              fill="var(--text-muted)"
            >
              {d.date.includes(':') ? d.date : d.date.slice(5)}
            </text>
          ) : null,
        )}

        {/* hover 命中区 */}
        {daily.map((_, i) => (
          <rect
            key={`h${i}`}
            x={PAD_L + slot * i}
            y={PAD_T}
            width={slot}
            height={plotH}
            fill="transparent"
            onMouseEnter={() => setHover(i)}
          />
        ))}
      </svg>

      {/* 悬浮详情（跟随命中列，过半宽自动翻到左侧） */}
      {hovered && hover !== null && (
        <div
          className="absolute top-2 z-10 pointer-events-none min-w-[168px] px-3 py-2.5 rounded-[var(--radius-md)] border border-[var(--border-strong)] bg-[var(--bg-overlay)] shadow-[var(--shadow-overlay)]"
          style={
            xMid(hover) <= width / 2
              ? { left: Math.round(xMid(hover)) + 10 }
              : { right: Math.round(width - xMid(hover)) + 10 }
          }
        >
          <div className="text-xs font-semibold text-[var(--text-primary)] mb-1.5">{hovered.date}</div>
          {(
            [
              ['var(--color-info)', t('usageStats.tip.totalTokens'), formatTokens(hovered.inputTokens + hovered.outputTokens + hovered.cacheReadTokens)],
              ['var(--color-success)', t('usageStats.tokens.in'), formatTokens(hovered.inputTokens)],
              ['var(--color-error)', t('usageStats.tokens.out'), formatTokens(hovered.outputTokens)],
              ['var(--color-warning)', t('usageStats.tokens.cached'), formatTokens(hovered.cacheReadTokens)],
              ['var(--accent)', t('usageStats.tip.cost'), formatCost(hovered.cost)],
              ['var(--text-muted)', t('usageStats.kpi.calls'), formatCount(hovered.calls)],
            ] as const
          ).map(([color, label, value]) => (
            <div key={label} className="flex items-center gap-2 py-px text-xs">
              <span className="w-1.5 h-1.5 rounded-full flex-shrink-0" style={{ backgroundColor: color }} />
              <span className="text-[var(--text-secondary)]">{label}</span>
              <span className="flex-1 text-right font-medium text-[var(--text-primary)] tabular-nums">
                {value}
              </span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
