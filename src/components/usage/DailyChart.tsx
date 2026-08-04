import { useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { useT } from '../../i18n';
import {
  Area,
  Bar,
  CartesianGrid,
  ComposedChart,
  Tooltip,
  XAxis,
  YAxis,
} from 'recharts';
import type { UsageDailyStat, UsageRange } from '../../types';
import { customChartWindow, rangeStartDate } from '../../utils/usageDates';
import { formatCost, formatCount, formatTokens } from './format';

function axisCost(v: number): string {
  return v >= 1000 ? `$${(v / 1000).toFixed(1)}K` : `$${v.toFixed(2)}`;
}

function dayKey(d: Date): string {
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`;
}

/** 补齐空桶：今天视图从 00:00 到当前小时；日粒度补窗口到今天，custom 补用户所选起止。
 * 后端快照是稀疏的（只有有数据的桶），无活动时段补 0 才能画出完整时间轴 */
function fillBuckets(
  daily: UsageDailyStat[],
  range: UsageRange,
  customFrom: string,
  customTo: string,
): UsageDailyStat[] {
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
  let end: Date;
  // 窗口起止与查询 since/until 同源(utils/usageDates 的范围规格);
  // custom 按用户所选起止补桶,轴反映所选窗口而非数据跨度
  const rangeStart = rangeStartDate(range, now);
  if (rangeStart) {
    start = rangeStart;
    end = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  } else {
    ({ start, end } = customChartWindow(customFrom, customTo, now));
  }
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

/** 悬浮详情：复用旧手绘版的行样式（色点 + 标签 + 右对齐数值）。
 * 签名只取用到的字段（TooltipContentProps 的父类型,参数逆变兼容 v3 泛型）。 */
function UsageTooltip({
  active,
  payload,
}: {
  active?: boolean;
  payload?: ReadonlyArray<{ payload?: unknown }>;
}) {
  const t = useT();
  if (!active || !payload || payload.length === 0) return null;
  const d = payload[0].payload as UsageDailyStat;
  return (
    <div className="pointer-events-none min-w-[168px] px-3 py-2.5 rounded-[var(--radius-md)] border border-[var(--border-strong)] bg-[var(--bg-overlay)] shadow-[var(--shadow-overlay)]">
      <div className="text-xs font-semibold text-[var(--text-primary)] mb-1.5">{d.date}</div>
      {(
        [
          ['var(--color-info)', t('usageStats.tip.totalTokens'), formatTokens(d.inputTokens + d.outputTokens + d.cacheReadTokens)],
          ['var(--color-success)', t('usageStats.tokens.in'), formatTokens(d.inputTokens)],
          ['var(--color-error)', t('usageStats.tokens.out'), formatTokens(d.outputTokens)],
          ['var(--color-warning)', t('usageStats.tokens.cached'), formatTokens(d.cacheReadTokens)],
          ['var(--accent)', t('usageStats.tip.cost'), formatCost(d.cost)],
          ['var(--text-muted)', t('usageStats.kpi.calls'), formatCount(d.calls)],
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
  );
}

const AXIS_TICK = { fontSize: 9, fill: 'var(--text-muted)' } as const;
// 全部图表配置提成常量:recharts 的 points 由 data+布局计算,margin 等每次
// render 新对象可能让 points 重算出新引用 → 动画被无谓重启(见 buckets memo 注释)
const CHART_MARGIN = { top: 10, right: 4, bottom: 0, left: 4 } as const;
const BAR_RADIUS: [number, number, number, number] = [2, 2, 0, 0];
const DOT_BIG = { r: 2.5, fill: 'var(--accent)', strokeWidth: 0 } as const;
const DOT_SMALL = { r: 1.8, fill: 'var(--accent)', strokeWidth: 0 } as const;
const ACTIVE_DOT = { r: 4, fill: 'var(--accent)', stroke: 'var(--bg-surface)', strokeWidth: 1.5 } as const;
const CURSOR = { stroke: 'var(--text-muted)', strokeDasharray: '3 3' } as const;
const tickDate = (v: string) => (v.includes(':') ? v : v.slice(5));
const tickCount = (v: number) => formatCount(Math.round(v));

/**
 * 时段活动图（recharts v3）：cost 面积（左轴）+ calls 柱（右轴），
 * 数据更新自带进入/补间动效。「今天」按小时分桶，其余按日历日；
 * 补齐后仍只有 1 个桶时退化为摘要卡（孤点图没有信息量）。
 * 宽度自测而不用 ResponsiveContainer：数据整包替换引发回流时 WKWebView 的
 * ResizeObserver 可能读到一拍 0 宽——RC 会就地渲染空图且不再自愈（表现为
 * 图表消失,再刷新才回来）；这里沿用旧手绘版的「忽略瞬时 0 宽」守卫。
 */
export function DailyChart({
  daily,
  range,
  customFrom,
  customTo,
}: {
  daily: UsageDailyStat[];
  range: UsageRange;
  /** custom range 起止("YYYY-MM-DD");其余 range 忽略 */
  customFrom: string;
  customTo: string;
}) {
  const t = useT();
  const containerRef = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);

  useLayoutEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const measure = () => {
      const w = el.clientWidth;
      if (w > 0) setWidth(w);
    };
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    measure();
    return () => ro.disconnect();
  }, []);

  let content: ReactNode;
  // recharts v3 按引用判定「数据变了→重启动画」(useAnimationId);buckets 必须
  // memo——每次 render 新建数组会让任何一次父组件重渲染都重启动画,连击下
  // 动画长期停在起始帧(clip 宽 0),表现为折线/柱整体消失
  const buckets = useMemo(
    () => (daily.length > 0 ? fillBuckets(daily, range, customFrom, customTo) : []),
    [daily, range, customFrom, customTo],
  );

  if (daily.length === 0) {
    content = (
      <div className="h-[232px] flex items-center justify-center text-sm text-[var(--text-muted)]">
        {t('usageStats.noDailyData')}
      </div>
    );
  } else if (buckets.length === 1) {
    // 补齐后仍单桶（如 0 点刚过打开「今天」）：摘要卡
    const d = buckets[0];
    content = (
      <div className="h-[232px] flex flex-col items-center justify-center gap-1.5">
        <div className="text-xs text-[var(--text-muted)]">{d.date}</div>
        <div className="text-3xl font-bold text-[var(--accent)]">{formatCost(d.cost)}</div>
        <div className="text-sm text-[var(--text-secondary)]">
          {t('usageStats.callsCount', { count: formatCount(d.calls) })}
        </div>
      </div>
    );
  } else if (width > 0) {
    const dot = buckets.length <= 40 ? DOT_BIG : buckets.length <= 90 ? DOT_SMALL : false;
    content = (
      <ComposedChart width={width} height={232} data={buckets} margin={CHART_MARGIN}>
        <defs>
          <linearGradient id="usage-daily-area" x1="0" y1="0" x2="0" y2="1">
            <stop offset="0%" stopColor="var(--accent)" stopOpacity="0.3" />
            <stop offset="100%" stopColor="var(--accent)" stopOpacity="0.02" />
          </linearGradient>
        </defs>
        {/* v3:多 YAxis 下 grid 必须显式指定 yAxisId,否则不渲染 */}
        <CartesianGrid
          yAxisId="cost"
          vertical={false}
          strokeDasharray="3 4"
          stroke="var(--border-default)"
        />
        <XAxis
          dataKey="date"
          tickFormatter={tickDate}
          tick={AXIS_TICK}
          axisLine={false}
          tickLine={false}
          minTickGap={24}
        />
        <YAxis
          yAxisId="cost"
          orientation="left"
          width={52}
          tickFormatter={axisCost}
          tick={AXIS_TICK}
          axisLine={false}
          tickLine={false}
        />
        <YAxis
          yAxisId="calls"
          orientation="right"
          width={44}
          tickFormatter={tickCount}
          tick={AXIS_TICK}
          axisLine={false}
          tickLine={false}
        />
        <Bar
          yAxisId="calls"
          dataKey="calls"
          fill="var(--text-muted)"
          opacity={0.28}
          radius={BAR_RADIUS}
          maxBarSize={14}
          animationDuration={400}
        />
        <Area
          yAxisId="cost"
          type="monotone"
          dataKey="cost"
          stroke="var(--accent)"
          strokeWidth={1.8}
          fill="url(#usage-daily-area)"
          dot={dot}
          activeDot={ACTIVE_DOT}
          animationDuration={400}
        />
        {/* SVG 无 z-index,层叠按 JSX 顺序,Tooltip 保持最后 */}
        <Tooltip content={UsageTooltip} cursor={CURSOR} />
      </ComposedChart>
    );
  }

  return (
    <div ref={containerRef} className="w-full">
      {content}
    </div>
  );
}
