import { useT } from '../../i18n';
import type { UsageTopSessionStat } from '../../types';
import { formatCost, formatCount } from './format';

interface Props {
  sessions: UsageTopSessionStat[];
  /** 点击行 → 复用 SessionViewerModal 查看会话正文 */
  onOpen: (s: UsageTopSessionStat) => void;
}

/** Top sessions 列表：日期 | 项目 | 标题 | 横条 | 成本 | 调用数 */
export function TopSessions({ sessions, onOpen }: Props) {
  const t = useT();
  if (sessions.length === 0) {
    return <div className="py-8 text-center text-sm text-[var(--text-muted)]">{t('usageStats.noSessions')}</div>;
  }
  // 横条基准：cost 榜首；全 $0 时（价格缺失）退化按 tokens
  const allZero = sessions.every((s) => s.cost <= 0);
  const max = Math.max(...sessions.map((s) => (allZero ? s.tokens : s.cost)), 1e-9);
  return (
    <div>
      {sessions.map((s) => (
        <button
          key={`${s.agent}-${s.sessionId}`}
          type="button"
          className="w-full flex items-center gap-3 py-[7px] px-2 -mx-2 rounded-[var(--radius-sm)] hover:bg-[var(--border-subtle)]/60 transition-colors text-left"
          onClick={() => onOpen(s)}
          title={s.title}
        >
          <span className="w-[76px] text-xs text-[var(--text-muted)] font-mono flex-shrink-0">
            {s.timestamp}
          </span>
          <span className="w-[150px] truncate text-[13px] text-[var(--text-secondary)] flex-shrink-0">
            {s.projectName}
          </span>
          <span className="flex-1 min-w-0 truncate text-[13px] text-[var(--text-primary)]">
            {s.title || t('usageStats.untitled')}
          </span>
          <span className="w-[110px] h-1.5 rounded-full bg-[var(--border-subtle)] overflow-hidden flex-shrink-0">
            <span
              className="block h-full rounded-full"
              style={{
                width: `${Math.max(((allZero ? s.tokens : s.cost) / max) * 100, 2)}%`,
                background: 'linear-gradient(90deg, var(--color-info), var(--color-ai))',
              }}
            />
          </span>
          <span className="w-14 text-right text-[13px] font-medium text-[var(--text-primary)] flex-shrink-0">
            {formatCost(s.cost)}
          </span>
          <span className="w-12 text-right text-xs text-[var(--text-muted)] flex-shrink-0">
            {formatCount(s.calls)}
          </span>
        </button>
      ))}
    </div>
  );
}
