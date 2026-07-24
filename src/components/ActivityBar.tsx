import { useAppStore, STATUS_PRIORITY, getHighestStatus } from '../store';
import { useT } from '../i18n';
import type { PaneStatus } from '../types';

const STATUS_COLORS: Record<PaneStatus, string> = {
  idle: 'var(--text-muted)',
  'ai-idle': 'var(--color-success)',
  'ai-working': 'var(--color-ai-working)',
  error: 'var(--color-error)',
};

// === 图标（统一 16 viewBox / stroke currentColor）===
const ICON_PANEL = (
  <svg width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <rect x="2" y="3" width="12" height="10" rx="1.5" />
    <path d="M6.5 3v10" />
  </svg>
);
const ICON_SESSIONS = (
  <svg width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M2 3h12v8H5l-3 3V3z" />
  </svg>
);
const ICON_GIT = (
  <svg width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <circle cx="5" cy="4" r="1.5" />
    <circle cx="11" cy="4" r="1.5" />
    <circle cx="5" cy="12" r="1.5" />
    <path d="M5 5.5v5M11 5.5v1a2 2 0 01-2 2H5" />
  </svg>
);
const ICON_SETTINGS = (
  <svg width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <circle cx="8" cy="8" r="2.1" />
    <path d="M8 1.5v1.6M8 12.9v1.6M14.5 8h-1.6M3.1 8H1.5M12.6 3.4l-1.1 1.1M4.5 11.5l-1.1 1.1M12.6 12.6l-1.1-1.1M4.5 4.5 3.4 3.4" />
  </svg>
);
const ICON_SSH = (
  <svg width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <rect x="2" y="3" width="12" height="10" rx="1.5" />
    <path d="M4.8 6.5 6.6 8l-1.8 1.5M8.4 10h2.8" />
  </svg>
);
const ICON_UPDATE = (
  <svg width="18" height="18" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M8 10.5V3M5 6l3-3 3 3" />
    <path d="M3 12.5h10" />
  </svg>
);

const ACCENT_BAR = (
  <span className="absolute left-0 top-1/2 -translate-y-1/2 w-0.5 h-4 rounded-full bg-[var(--accent)]" />
);

interface ActivityBarProps {
  onOpenSettings: () => void;
  onOpenSsh: () => void;
  /** 有新版本时的版本号（null = 无更新,不显示更新按钮） */
  updateVersion?: string | null;
  onOpenUpdate: () => void;
}

export function ActivityBar({ onOpenSettings, onOpenSsh, updateVersion, onOpenUpdate }: ActivityBarProps) {
  const t = useT();
  const config = useAppStore((s) => s.config);
  const projectStates = useAppStore((s) => s.projectStates);
  const rightDrawer = useAppStore((s) => s.rightDrawer);
  const toggleMiddleColumn = useAppStore((s) => s.toggleMiddleColumn);
  const toggleRightDrawer = useAppStore((s) => s.toggleRightDrawer);

  const middleVisible = config.middleColumnVisible;

  // 聚合所有项目的最高 AI 状态（徽标挂在「中间栏」按钮上,中间栏承载 Projects）
  let globalStatus: PaneStatus = 'idle';
  for (const ps of projectStates.values()) {
    for (const tab of ps.tabs) {
      const s = getHighestStatus(tab.splitLayout);
      if (STATUS_PRIORITY[s] > STATUS_PRIORITY[globalStatus]) {
        globalStatus = s;
      }
    }
  }

  const btnClass = (active: boolean) =>
    `relative w-8 h-8 flex items-center justify-center rounded transition-colors ${
      active
        ? 'text-[var(--text-primary)] bg-[var(--border-subtle)]'
        : 'text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]/50'
    }`;

  return (
    <div
      className="h-full bg-[var(--bg-surface)] flex flex-col items-center pt-2 pb-2 gap-1 border-r border-[var(--border-subtle)] select-none"
      style={{ width: 44 }}
    >
      {/* 折叠 / 展开中间栏 */}
      <button
        className={btnClass(middleVisible)}
        onClick={toggleMiddleColumn}
        title={middleVisible ? t('app.activityBar.collapse') : t('app.activityBar.expand')}
      >
        {ICON_PANEL}
        {middleVisible && ACCENT_BAR}
        {globalStatus !== 'idle' && (
          <span
            className={`absolute -top-0.5 -right-0.5 w-2 h-2 rounded-full border border-[var(--bg-surface)] ${
              globalStatus === 'ai-working' ? 'animate-blink' : ''
            }`}
            style={{ backgroundColor: STATUS_COLORS[globalStatus] }}
          />
        )}
      </button>

      {/* Sessions 抽屉 */}
      <button
        className={btnClass(rightDrawer === 'sessions')}
        onClick={() => toggleRightDrawer('sessions')}
        title={t('app.activityBar.sessions')}
      >
        {ICON_SESSIONS}
        {rightDrawer === 'sessions' && ACCENT_BAR}
      </button>

      {/* Git 抽屉 */}
      <button
        className={btnClass(rightDrawer === 'git')}
        onClick={() => toggleRightDrawer('git')}
        title={t('app.activityBar.git')}
      >
        {ICON_GIT}
        {rightDrawer === 'git' && ACCENT_BAR}
      </button>

      {/* 分隔符 */}
      <div className="w-6 h-px bg-[var(--border-default)] my-1" />

      {/* 设置 */}
      <button className={btnClass(false)} onClick={onOpenSettings} title={t('app.activityBar.settings')}>
        {ICON_SETTINGS}
      </button>
      {/* SSH */}
      <button className={btnClass(false)} onClick={onOpenSsh} title={t('app.activityBar.ssh')}>
        {ICON_SSH}
      </button>

      {/* 有新版本时才出现：点击前往下载 */}
      {updateVersion && (
        <button
          className="relative w-8 h-8 flex items-center justify-center rounded text-[var(--accent)] hover:bg-[var(--accent)]/15 transition-colors"
          onClick={onOpenUpdate}
          title={t('app.update.title', { version: updateVersion })}
        >
          {ICON_UPDATE}
          <span className="absolute -top-0.5 -right-0.5 w-2 h-2 rounded-full bg-[var(--accent)] border border-[var(--bg-surface)] animate-blink" />
        </button>
      )}
    </div>
  );
}
