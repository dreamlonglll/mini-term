import { useState, useEffect, useCallback } from 'react';
import { useAppStore } from '../store';
import { SessionList } from './SessionList';
import { GitHistory } from './GitHistory';
import { useT } from '../i18n';

const MIN_WIDTH = 240;
const MAX_WIDTH = 720;

interface RightDrawerProps {
  /** 初始宽度（来自持久化 config），拖拽期间由本组件自持,松手时回调 onResizeEnd */
  initialWidth: number;
  onResizeEnd: (width: number) => void;
}

const clamp = (w: number) => Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, w));

/**
 * 右侧悬浮抽屉：从右边缘滑出、浮在终端之上,互斥承载 Sessions / Git 单面板。
 * 开合态取自 store.rightDrawer（运行时,不持久化）;宽度持久化到 config。
 */
export function RightDrawer({ initialWidth, onResizeEnd }: RightDrawerProps) {
  const t = useT();
  const rightDrawer = useAppStore((s) => s.rightDrawer);
  const closeRightDrawer = useAppStore((s) => s.closeRightDrawer);
  const openRightDrawer = useAppStore((s) => s.openRightDrawer);
  const [width, setWidth] = useState(clamp(initialWidth));

  // config 侧宽度变化（或初次持久值到达）时同步,拖拽自身不受影响
  useEffect(() => {
    setWidth(clamp(initialWidth));
  }, [initialWidth]);

  const startResize = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startWidth = width;
    // 拖拽期间禁止页面选中文本(否则划过终端会误选)
    const prevUserSelect = document.body.style.userSelect;
    document.body.style.userSelect = 'none';
    // 抽屉贴右边缘:把左缘往左拖 => 变宽,故用 startX - 当前X
    const onMove = (ev: MouseEvent) => setWidth(clamp(startWidth + (startX - ev.clientX)));
    const onUp = (ev: MouseEvent) => {
      document.removeEventListener('mousemove', onMove);
      document.removeEventListener('mouseup', onUp);
      document.body.style.userSelect = prevUserSelect;
      const final = clamp(startWidth + (startX - ev.clientX));
      setWidth(final);
      onResizeEnd(final);
    };
    document.addEventListener('mousemove', onMove);
    document.addEventListener('mouseup', onUp);
  }, [width, onResizeEnd]);

  if (!rightDrawer) return null;

  return (
    <div
      className="absolute top-0 right-0 h-full z-30 flex flex-col bg-[var(--bg-surface)] border-l border-[var(--border-default)] shadow-[var(--shadow-overlay)]"
      style={{ width }}
    >
      {/* 左缘拖拽手柄 */}
      <div
        className="absolute left-0 top-0 h-full w-1.5 -translate-x-1/2 cursor-col-resize hover:bg-[var(--accent)]/40 transition-colors z-10"
        onMouseDown={startResize}
      />
      {/* 标题条：两个面板互斥，之前只有一个 ✕，切换必须回 ActivityBar 点两下。
          这里直接给一组 segmented 切换，抽屉内一次点击就能换面板。 */}
      <div className="flex items-center gap-1 h-9 px-1.5 border-b border-[var(--border-subtle)] flex-shrink-0">
        <div
          role="tablist"
          aria-label={t('app.activityBar.sessions')}
          className="flex flex-1 rounded-[var(--radius-sm)] border border-[var(--border-default)] overflow-hidden"
        >
          {(['sessions', 'git'] as const).map((panel) => (
            <button
              key={panel}
              type="button"
              role="tab"
              aria-selected={rightDrawer === panel}
              className={`flex-1 px-2 py-1 text-xs transition-colors ${
                rightDrawer === panel
                  ? 'bg-[var(--accent-subtle)] text-[var(--accent)] font-medium'
                  : 'text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)]'
              }`}
              onClick={() => openRightDrawer(panel)}
            >
              {panel === 'sessions' ? t('panels.sessions') : t('panels.git')}
            </button>
          ))}
        </div>
        <button
          type="button"
          className="w-6 h-6 flex items-center justify-center rounded-[var(--radius-sm)] text-[var(--text-muted)] hover:text-[var(--text-primary)] hover:bg-[var(--border-subtle)] transition-colors flex-shrink-0"
          onClick={closeRightDrawer}
          title={t('app.activityBar.closeDrawer')}
          aria-label={t('app.activityBar.closeDrawer')}
        >
          <svg width="11" height="11" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round">
            <path d="M4.5 4.5l7 7M11.5 4.5l-7 7" />
          </svg>
        </button>
      </div>
      <div className="flex-1 min-h-0 overflow-hidden">
        {rightDrawer === 'sessions' ? <SessionList /> : <GitHistory />}
      </div>
    </div>
  );
}
