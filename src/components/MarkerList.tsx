import type { AiMarker } from '../types';
import { scrollToMarker } from '../utils/terminalCache';
import { useT } from '../i18n';

interface Props {
  ptyId: number;
  markers: AiMarker[];
  onClose: () => void;
}

function formatTime(ts: number): string {
  const d = new Date(ts);
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

function truncate(s: string, max = 40): string {
  return s.length <= max ? s : s.slice(0, max - 1) + '…';
}

export function MarkerList({ ptyId, markers, onClose }: Props) {
  const t = useT();
  if (markers.length === 0) {
    return (
      <div className="px-3 py-2 text-xs text-[var(--text-muted)]">
        {t("markerList.empty")}
      </div>
    );
  }
  return (
    <div className="max-h-80 overflow-y-auto py-1 min-w-[280px]">
      {markers.map((m) => (
        <button
          key={m.id}
          className="w-full text-left px-3 py-1.5 text-xs flex items-center gap-2 hover:bg-[var(--bg-hover)]"
          title={m.line}
          onClick={() => {
            scrollToMarker(ptyId, m.xtermMarkerId);
            onClose();
          }}
        >
          <span className="text-[var(--text-muted)] tabular-nums w-8">#{m.seq}</span>
          <span className="text-[var(--text-muted)] tabular-nums w-10">{formatTime(m.ts)}</span>
          <span className="flex-1 truncate">{truncate(m.line)}</span>
          {m.inProgress && (
            <span
              className="w-1.5 h-1.5 rounded-full shrink-0"
              style={{ background: 'var(--color-ai-working)' }}
              aria-label={t("markerList.inProgress")}
            />
          )}
        </button>
      ))}
    </div>
  );
}
