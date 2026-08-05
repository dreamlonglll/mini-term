export interface RankRow {
  key: string;
  label: string;
  /** 横条占比 0..1（相对榜首） */
  ratio: number;
  /** 右侧主值（如 "$796"） */
  primary: string;
  /** 右侧次值（如会话数） */
  secondary?: string;
  /** hover 提示（完整路径等） */
  title?: string;
  /** 可点击行（如项目排行点击切入单项目 scope） */
  onClick?: () => void;
}

/** 横条排行通用件（项目排行等复用）：label | 渐变横条 | 主值 | 次值 */
export function RankBarList({ rows, emptyText }: { rows: RankRow[]; emptyText: string }) {
  if (rows.length === 0) {
    return <div className="py-8 text-center text-sm text-[var(--text-muted)]">{emptyText}</div>;
  }
  return (
    <div>
      {rows.map((r) => (
        <div
          key={r.key}
          className={`flex items-center gap-3 py-[7px] ${
            r.onClick
              ? 'cursor-pointer -mx-1.5 px-1.5 rounded-[var(--radius-sm)] hover:bg-[var(--border-subtle)] transition-colors'
              : ''
          }`}
          title={r.title}
          onClick={r.onClick}
        >
          <span className="flex-1 min-w-0 truncate text-[13px] text-[var(--text-secondary)]">
            {r.label}
          </span>
          <span className="w-14 h-1.5 rounded-full bg-[var(--border-subtle)] overflow-hidden flex-shrink-0">
            <span
              className="usage-rank-bar block h-full rounded-full transition-[width] duration-500 ease-out"
              style={{
                width: `${Math.max(Math.min(r.ratio, 1) * 100, 2)}%`,
                background: 'linear-gradient(90deg, var(--color-info), var(--color-ai))',
              }}
            />
          </span>
          <span className="w-14 text-right text-[13px] font-medium text-[var(--text-primary)] flex-shrink-0">
            {r.primary}
          </span>
          {r.secondary !== undefined && (
            <span className="w-10 text-right text-xs text-[var(--text-muted)] flex-shrink-0">
              {r.secondary}
            </span>
          )}
        </div>
      ))}
    </div>
  );
}
