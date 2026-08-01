import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { Modal, ModalCloseButton } from '../Modal';
import { SessionViewerModal } from '../SessionViewerModal';
import { useUsageStats } from '../../hooks/useUsageStats';
import { useT } from '../../i18n';
import type { AiSession, UsageAgentFilter, UsageModelStat, UsageRange, UsageTopSessionStat } from '../../types';
import { KpiCards } from './KpiCards';
import { DailyChart } from './DailyChart';
import { RankBarList, type RankRow } from './RankBarList';
import { TopSessions } from './TopSessions';
import { formatCost, formatTokens, modelShortName } from './format';

const SCOPE_KEY = 'mini-term-usage-scope';
const RANGE_KEY = 'mini-term-usage-range';
const AUTO_REFRESH_KEY = 'mini-term-usage-autorefresh';

/** 自动刷新档位（秒）；0 = 关闭 */
const AUTO_REFRESH_OPTIONS = [0, 5, 10, 30, 60] as const;
const TOP_MODELS = 6;

function loadPref<T extends string>(key: string, valid: readonly T[], fallback: T): T {
  try {
    const v = localStorage.getItem(key);
    if (v && (valid as readonly string[]).includes(v)) return v as T;
  } catch {
    /* localStorage 不可用则用默认值 */
  }
  return fallback;
}

function savePref(key: string, v: string) {
  try {
    localStorage.setItem(key, v);
  } catch {
    /* 持久化失败不影响本次使用 */
  }
}

const SCOPES: readonly UsageAgentFilter[] = ['all', 'claude', 'codex'];
const RANGES: readonly UsageRange[] = ['today', 'days7', 'days30', 'all'];

function Segmented<T extends string>({
  options,
  value,
  onChange,
  labelOf,
}: {
  options: readonly T[];
  value: T;
  onChange: (v: T) => void;
  labelOf: (v: T) => string;
}) {
  return (
    <div className="flex rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden text-xs flex-shrink-0">
      {options.map((opt) => (
        <button
          key={opt}
          type="button"
          className={`px-2.5 py-1 transition-colors whitespace-nowrap ${
            value === opt
              ? 'bg-[var(--accent)] text-[var(--bg-base)]'
              : 'text-[var(--text-muted)] hover:text-[var(--text-primary)]'
          }`}
          onClick={() => onChange(opt)}
        >
          {labelOf(opt)}
        </button>
      ))}
    </div>
  );
}

/** 区块卡片：左侧竖条标题 + 内容 */
function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div className="border border-[var(--border-subtle)] rounded-[var(--radius-md)] bg-[var(--bg-elevated)]/40 px-4 py-3.5">
      <div className="flex items-center gap-2 mb-3">
        <span className="w-0.5 h-3.5 rounded-full bg-[var(--color-info)]" />
        <span className="text-sm font-semibold text-[var(--text-primary)]">{title}</span>
      </div>
      {children}
    </div>
  );
}

const ICON_REFRESH = (
  <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
    <path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9M13.5 2.5v2.7h-2.7" />
  </svg>
);

export function UsageStatsModal({ open, onClose }: { open: boolean; onClose: () => void }) {
  const t = useT();
  const [scope, setScope] = useState<UsageAgentFilter>(() => loadPref(SCOPE_KEY, SCOPES, 'all'));
  const [range, setRange] = useState<UsageRange>(() => loadPref(RANGE_KEY, RANGES, 'days30'));
  const [autoRefresh, setAutoRefresh] = useState<number>(() => {
    const v = Number(loadPref(AUTO_REFRESH_KEY, AUTO_REFRESH_OPTIONS.map(String), '0'));
    return Number.isFinite(v) ? v : 0;
  });
  const [viewer, setViewer] = useState<UsageTopSessionStat | null>(null);

  const { phase, stats, processed, total, error, refresh } = useUsageStats(open, scope, range);

  // 自动刷新：仅在上一轮扫描完成后触发（静默重扫），间隔内还在扫则跳过本拍，
  // 避免扫描时长 > 间隔时永远扫不完
  const phaseRef = useRef(phase);
  phaseRef.current = phase;
  useEffect(() => {
    if (!open || autoRefresh <= 0) return;
    const id = window.setInterval(() => {
      if (phaseRef.current === 'done') refresh();
    }, autoRefresh * 1000);
    return () => window.clearInterval(id);
  }, [open, autoRefresh, refresh]);

  const changeScope = useCallback((v: UsageAgentFilter) => {
    setScope(v);
    savePref(SCOPE_KEY, v);
  }, []);
  const changeRange = useCallback((v: UsageRange) => {
    setRange(v);
    savePref(RANGE_KEY, v);
  }, []);
  const changeAutoRefresh = useCallback((v: number) => {
    setAutoRefresh(v);
    savePref(AUTO_REFRESH_KEY, String(v));
  }, []);

  const scopeLabel = (v: UsageAgentFilter) =>
    v === 'all' ? t('usageStats.scope.all') : v === 'claude' ? 'Claude' : 'Codex';
  const rangeLabel = (v: UsageRange) => t(`usageStats.range.${v}`);

  /** 按模型排行：前 6 + Others 合并；全 $0 时按 tokens 排比例 */
  const modelRows = (models: UsageModelStat[], totalCost: number): RankRow[] => {
    const metric = (m: { cost: number; tokens: number }) => (totalCost > 0 ? m.cost : m.tokens);
    const top = models.slice(0, TOP_MODELS);
    const rest = models.slice(TOP_MODELS);
    const rows: RankRow[] = top.map((m) => ({
      key: m.model || '(unknown)',
      label: m.model ? modelShortName(m.model) : t('usageStats.unknownModel'),
      ratio: 0,
      primary: formatCost(m.cost),
      secondary: formatTokens(m.tokens),
      title: m.model || undefined,
    }));
    if (rest.length > 0) {
      const others = rest.reduce(
        (acc, m) => ({ cost: acc.cost + m.cost, tokens: acc.tokens + m.tokens }),
        { cost: 0, tokens: 0 },
      );
      rows.push({
        key: '(others)',
        label: t('usageStats.othersModels', { count: rest.length }),
        ratio: 0,
        primary: formatCost(others.cost),
        secondary: formatTokens(others.tokens),
      });
      top.push({ model: '', ...others, calls: 0 });
    }
    const max = Math.max(...top.map(metric), 1e-9);
    return rows.map((r, i) => ({ ...r, ratio: metric(top[i]) / max }));
  };

  // 状态优先级（互斥渲染）：价格失败(Retry) ＞ 价格加载中 ＞ 扫描错误 ＞
  // 骨架(无 partial) ＞ 空态 ＞ 主体。价格未就绪时绝不渲染 KPI(全 0 会误导)
  let body: ReactNode;
  if (phase === 'pricingError') {
    body = (
      <StateHint
        text={t('usageStats.pricingError')}
        detail={error}
        action={{ label: t('usageStats.retry'), onClick: refresh }}
      />
    );
  } else if (phase === 'pricing') {
    body = <StateHint text={t('usageStats.pricingLoading')} spinning />;
  } else if (phase === 'error') {
    body = (
      <StateHint
        text={t('usageStats.scanError')}
        detail={error}
        action={{ label: t('usageStats.retry'), onClick: refresh }}
      />
    );
  } else if (!stats) {
    body = <StateHint text={t('usageStats.scanning')} spinning />;
  } else if (phase === 'done' && stats.sessionCount === 0) {
    body = <StateHint text={t('usageStats.empty')} />;
  } else {
    /** 排行横条比例：相对榜首；成本全 $0（价格缺失）时按 tokens */
    const metric = (x: { cost: number; tokens: number }) => (stats.totalCost > 0 ? x.cost : x.tokens);
    const ratioOf = (x: { cost: number; tokens: number }, first?: { cost: number; tokens: number }) =>
      first && metric(first) > 0 ? metric(x) / metric(first) : 0;

    body = (
      <div className="space-y-4">
        <KpiCards stats={stats} />

        {/* Token 副行：in / out / cached / written */}
        <div className="flex items-center gap-3 text-[13px] text-[var(--text-muted)] px-1">
          {(
            [
              [stats.inputTokens, 'in'],
              [stats.outputTokens, 'out'],
              [stats.cacheReadTokens, 'cached'],
              [stats.cacheWriteTokens, 'written'],
            ] as const
          ).map(([v, key], i) => (
            <span key={key} className="flex items-center gap-3">
              {i > 0 && <span className="text-[var(--border-strong)]">|</span>}
              <span>
                <span className="font-semibold text-[var(--text-primary)]">{formatTokens(v)}</span>{' '}
                {t(`usageStats.tokens.${key}`)}
              </span>
            </span>
          ))}
        </div>

        {/* 使用趋势：全宽（hover 显示时段详情） */}
        <Section title={t('usageStats.dailyActivity')}>
          <DailyChart daily={stats.daily} range={range} />
        </Section>

        {/* 项目 | 模型 | 供应商 三卡同行；项目数可能多，固定高度内滚动 */}
        <div className="flex gap-4 items-start">
          <div className="flex-1 min-w-0">
            <Section title={t('usageStats.byProject')}>
              <div className="max-h-[216px] overflow-y-auto">
                <RankBarList
                  emptyText={t('usageStats.noSessions')}
                  rows={stats.byProject.map((p) => ({
                    key: p.path || p.name,
                    label: p.name,
                    ratio: ratioOf(p, stats.byProject[0]),
                    primary: formatCost(p.cost),
                    secondary: String(p.sessions),
                    title: p.path,
                  }))}
                />
              </div>
            </Section>
          </div>
          <div className="flex-1 min-w-0">
            <Section title={t('usageStats.byModel')}>
              <RankBarList
                emptyText={t('usageStats.noSessions')}
                rows={modelRows(stats.byModel, stats.totalCost)}
              />
            </Section>
          </div>
          <div className="flex-1 min-w-0">
            <Section title={t('usageStats.byProvider')}>
              <RankBarList
                emptyText={t('usageStats.noSessions')}
                rows={stats.byProvider.map((p) => ({
                  key: p.provider || '(unknown)',
                  label: p.provider || t('usageStats.unknownProvider'),
                  ratio: ratioOf(p, stats.byProvider[0]),
                  primary: formatCost(p.cost),
                  secondary: formatTokens(p.tokens),
                  title: p.provider || undefined,
                }))}
              />
            </Section>
          </div>
        </div>

        <Section title={t('usageStats.topSessions')}>
          <TopSessions sessions={stats.topSessions} onOpen={setViewer} />
        </Section>
      </div>
    );
  }

  const viewerSession: AiSession | null = viewer
    ? {
        id: viewer.sessionId,
        sessionType: viewer.agent === 'codex' ? 'codex' : 'claude',
        title: viewer.title,
        timestamp: viewer.timestamp,
      }
    : null;

  return (
    <>
      <Modal
        open={open}
        onClose={onClose}
        panelClassName="w-[960px] max-h-[85vh]"
        ariaLabel={t('usageStats.title')}
      >
        {/* 自定义头部：标题 + Scope/Range 分段 + 刷新 + 关闭 */}
        <div className="flex items-center gap-3 px-5 py-3.5 border-b border-[var(--border-subtle)] flex-shrink-0">
          <h2 className="text-base font-semibold text-[var(--text-primary)] flex-shrink-0">
            {t('usageStats.title')}
          </h2>
          <div className="flex-1 flex items-center justify-center gap-3 min-w-0">
            <Segmented options={SCOPES} value={scope} onChange={changeScope} labelOf={scopeLabel} />
            <Segmented options={RANGES} value={range} onChange={changeRange} labelOf={rangeLabel} />
          </div>
          {/* 自动刷新间隔（紧挨手动刷新按钮，语义自明） */}
          <select
            className="bg-[var(--bg-base)] border border-[var(--border-default)] rounded-[var(--radius-sm)] px-1.5 py-1 text-xs text-[var(--text-secondary)] focus:outline-none focus:border-[var(--accent)] flex-shrink-0"
            value={autoRefresh}
            onChange={(e) => changeAutoRefresh(Number(e.target.value))}
            title={t('usageStats.autoRefresh')}
          >
            {AUTO_REFRESH_OPTIONS.map((s) => (
              <option key={s} value={s}>
                {s === 0 ? t('usageStats.autoRefreshOff') : `${s}s`}
              </option>
            ))}
          </select>
          <button
            type="button"
            className="w-7 h-7 flex items-center justify-center rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors flex-shrink-0"
            onClick={refresh}
            title={t('usageStats.refresh')}
          >
            {ICON_REFRESH}
          </button>
          <ModalCloseButton onClose={onClose} />
        </div>

        <div className="flex-1 overflow-y-auto px-5 py-4">
          {/* 副标题 + 扫描进度 */}
          <div className="flex items-center justify-between mb-3">
            <div className="text-sm text-[var(--text-secondary)]">
              <span className="font-semibold text-[var(--text-primary)]">{scopeLabel(scope)}</span>
              <span className="mx-1.5 text-[var(--text-muted)]">·</span>
              {rangeLabel(range)}
            </div>
            {phase === 'scanning' && total > 0 && (
              <div className="text-xs text-[var(--text-muted)] tabular-nums">
                {t('usageStats.progress', { processed, total })}
              </div>
            )}
          </div>
          {body}
        </div>
      </Modal>

      {/* Top session 点击 → 复用现有会话查看器（嵌套弹窗，Esc 只关最上层） */}
      <SessionViewerModal
        open={viewer !== null}
        onClose={() => setViewer(null)}
        session={viewerSession}
        projectPath={viewer?.projectPath ?? ''}
      />
    </>
  );
}

function StateHint({
  text,
  detail,
  spinning,
  action,
}: {
  text: string;
  detail?: string;
  spinning?: boolean;
  action?: { label: string; onClick: () => void };
}) {
  return (
    <div className="py-20 flex flex-col items-center gap-3">
      {spinning && (
        <span className="w-5 h-5 border-2 border-[var(--border-strong)] border-t-[var(--accent)] rounded-full animate-spin" />
      )}
      <div className="text-sm text-[var(--text-secondary)]">{text}</div>
      {detail && (
        <div className="text-xs text-[var(--text-muted)] max-w-[480px] truncate" title={detail}>
          {detail}
        </div>
      )}
      {action && (
        <button
          type="button"
          className="px-3 py-1.5 text-xs rounded-[var(--radius-sm)] bg-[var(--accent)] text-[var(--bg-base)] hover:opacity-90 transition-opacity"
          onClick={action.onClick}
        >
          {action.label}
        </button>
      )}
    </div>
  );
}
