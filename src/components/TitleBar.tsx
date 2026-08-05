import { useCallback, useEffect, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { useAppStore } from '../store';
import { useTauriEvent } from '../hooks/useTauriEvent';
import { collectPanes } from '../utils/layoutOps';
import { focusAttentionTarget } from '../utils/attentionJump';
import { isMac, isWindows } from '../utils/platform';
import { useT } from '../i18n';
import type { ProjectState } from '../types';

/** 标题栏高度。Windows 原生标题栏是 32px，跟齐它，窗口按钮的手感才对得上。 */
export const TITLE_BAR_HEIGHT = 32;
/** macOS 交通灯占位：三颗灯 + 左右留白，内容从这条线之后开始。 */
const MAC_TRAFFIC_LIGHT_WIDTH = 78;

// 窗口控制图标：Windows 的画法是 10×10 内的细线，不是 Material 那种粗描边。
// 三个图标共用 10 viewBox，线宽 1，保证并排时视觉重量一致。
const ICON_MINIMIZE = (
  <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" strokeWidth="1">
    <path d="M0 5.5h10" />
  </svg>
);
const ICON_MAXIMIZE = (
  <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" strokeWidth="1">
    <rect x="0.5" y="0.5" width="9" height="9" />
  </svg>
);
// 还原态：后面那层窗口只画露出来的两条边，画成完整方框会糊成一团
const ICON_RESTORE = (
  <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" strokeWidth="1">
    <rect x="0.5" y="2.5" width="7" height="7" />
    <path d="M2.5 2.5V0.5h7v7h-2" />
  </svg>
);
const ICON_CLOSE = (
  <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" strokeWidth="1">
    <path d="M0.5 0.5l9 9M9.5 0.5l-9 9" />
  </svg>
);

const ICON_LOGO = (
  <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
    <rect x="1.5" y="2.5" width="13" height="11" rx="1.5" />
    <path d="M4.5 6.5L6.5 8l-2 1.5M8.5 10h3" />
  </svg>
);

/** 状态灯的四档。按紧急度取全局最高一档，颜色沿用托盘/项目列表那套语义色。 */
type LightKind = 'error' | 'attention' | 'working' | 'done' | 'idle';

const LIGHT_COLORS: Record<LightKind, string> = {
  error: 'var(--color-error)',
  attention: 'var(--color-warning)',
  working: 'var(--color-ai-working)',
  done: 'var(--color-success)',
  idle: 'var(--text-muted)',
};

const LIGHT_ORDER: Record<LightKind, number> = {
  error: 4,
  attention: 3,
  working: 2,
  done: 1,
  idle: 0,
};

/** 全局状态灯取所有项目所有 pane 里最紧急的一档。 */
function computeLight(
  projectStates: Map<string, ProjectState>,
  aiDoneOrder: Map<string, number>,
): LightKind {
  let light: LightKind = 'idle';
  const bump = (kind: LightKind) => {
    if (LIGHT_ORDER[kind] > LIGHT_ORDER[light]) light = kind;
  };
  for (const ps of projectStates.values()) {
    if (!ps.layout) continue;
    for (const pane of collectPanes(ps.layout)) {
      if (pane.status === 'error') bump('error');
      else if (pane.attention) bump('attention');
      else if (pane.status === 'ai-working') bump('working');
      else if (aiDoneOrder.has(pane.id)) bump('done');
    }
  }
  return light;
}

interface Props {
  /** 当前应用版本号（App 启动时取到后传入）；空串 = 还没取到，先不显示 */
  version: string;
}

export function TitleBar({ version }: Props) {
  const t = useT();
  const projectStates = useAppStore((s) => s.projectStates);
  const aiDoneOrder = useAppStore((s) => s.aiDoneOrder);
  const [maximized, setMaximized] = useState(false);
  // Windows 上最大化按钮那块是「非客户区」，鼠标消息不进 WebView，
  // CSS :hover 失效，悬停态只能由后端命中测试回传（见 window_snap.rs）
  const [ncMaxHover, setNcMaxHover] = useState(false);
  const maxButtonRef = useRef<HTMLButtonElement>(null);

  const light = computeLight(projectStates, aiDoneOrder);

  useEffect(() => {
    const appWindow = getCurrentWindow();
    let disposed = false;
    const sync = () => {
      appWindow.isMaximized().then((v) => {
        if (!disposed) setMaximized(v);
      }).catch(() => {});
    };
    sync();
    const unlisten = appWindow.onResized(sync);
    return () => {
      disposed = true;
      unlisten.then((fn) => fn()).catch(() => {});
    };
  }, []);

  useTauriEvent<boolean>('titlebar-max-hover', useCallback((hovering: boolean) => {
    setNcMaxHover(hovering);
  }, []));

  // Windows：把最大化按钮的位置报给后端，换回 Win11 的贴靠布局菜单。
  // 报完这块区域就归系统管了，React 的 onClick 再也收不到——最大化改由
  // window_snap.rs 收到 WM_NCLBUTTONUP 时直接投 WM_SYSCOMMAND 完成。
  useEffect(() => {
    if (!isWindows) return;
    const report = () => {
      const rect = maxButtonRef.current?.getBoundingClientRect();
      if (!rect) return;
      invoke('set_max_button_rect', {
        x: rect.left,
        y: rect.top,
        width: rect.width,
        height: rect.height,
      }).catch(() => {});
    };
    report();
    // 按钮自身尺寸（字号/缩放）与窗口宽度（决定它的 x）都会挪动这个矩形
    const observer = new ResizeObserver(report);
    if (maxButtonRef.current) observer.observe(maxButtonRef.current);
    observer.observe(document.documentElement);
    return () => {
      observer.disconnect();
      // 撤销上报：组件卸载后那块区域必须还给 WebView，否则残留一片点不动的死区
      invoke('set_max_button_rect', { x: 0, y: 0, width: 0, height: 0 }).catch(() => {});
    };
  }, []);

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    if (e.button !== 0) return;
    if ((e.target as HTMLElement).closest('[data-no-drag]')) return;
    // -webkit-app-region 会让 WebView2 进模态循环、外部工具一介入就锁住输入
    // （v0.2.16 修过一轮），拖拽一律走 Tauri API
    const appWindow = getCurrentWindow();
    if (e.detail === 2) {
      void appWindow.toggleMaximize();
      return;
    }
    void appWindow.startDragging();
  }, []);

  const buttonClass =
    'w-[46px] h-full flex items-center justify-center text-[var(--text-secondary)] ' +
    'hover:bg-[var(--border-default)] hover:text-[var(--text-primary)] transition-colors';

  return (
    <div
      data-titlebar
      className="flex items-stretch shrink-0 select-none bg-[var(--bg-surface)] border-b border-[var(--border-subtle)]"
      style={{ height: TITLE_BAR_HEIGHT }}
      onMouseDown={handleMouseDown}
    >
      {/* macOS 把左上角让给系统交通灯 */}
      {isMac && <div style={{ width: MAC_TRAFFIC_LIGHT_WIDTH }} />}

      <div className="flex items-center gap-1.5 px-3 text-[var(--text-muted)]">
        {ICON_LOGO}
        <span className="text-xs font-medium tracking-wide text-[var(--text-secondary)]">
          Mini-Term
        </span>
        {version && <span className="text-[11px] text-[var(--text-muted)]">v{version}</span>}
      </div>

      {/* 中段留白 —— 这里是主要的拖拽区 */}
      <div className="flex-1" />

      {/* 全局状态灯：点一下跳到下一个该处理的会话 */}
      <button
        data-no-drag
        type="button"
        className="px-2.5 flex items-center justify-center group"
        title={t(`app.titleBar.status.${light}`)}
        aria-label={t(`app.titleBar.status.${light}`)}
        onClick={() => focusAttentionTarget()}
      >
        <span
          className={`w-2 h-2 rounded-full transition-transform group-hover:scale-125 ${
            light === 'working' ? 'animate-blink' : ''
          }`}
          style={{
            backgroundColor: LIGHT_COLORS[light],
            opacity: light === 'idle' ? 0.45 : 1,
          }}
        />
      </button>

      {/* 窗口控制 —— macOS 用系统交通灯，这里不画 */}
      {!isMac && (
        <div data-no-drag className="flex items-stretch">
          <button
            type="button"
            className={buttonClass}
            title={t('app.titleBar.minimize')}
            aria-label={t('app.titleBar.minimize')}
            onClick={() => void getCurrentWindow().minimize()}
          >
            {ICON_MINIMIZE}
          </button>
          <button
            ref={maxButtonRef}
            type="button"
            className={`${buttonClass} ${ncMaxHover ? 'bg-[var(--border-default)] !text-[var(--text-primary)]' : ''}`}
            title={maximized ? t('app.titleBar.restore') : t('app.titleBar.maximize')}
            aria-label={maximized ? t('app.titleBar.restore') : t('app.titleBar.maximize')}
            // Windows 上这个 onClick 收不到事件（区域已交给系统），留着是给
            // Linux 以及命中测试没装上时的降级路径用
            onClick={() => void getCurrentWindow().toggleMaximize()}
          >
            {maximized ? ICON_RESTORE : ICON_MAXIMIZE}
          </button>
          <button
            type="button"
            className="w-[46px] h-full flex items-center justify-center text-[var(--text-secondary)] hover:bg-[#c42b1c] hover:text-white transition-colors"
            title={t('app.titleBar.close')}
            aria-label={t('app.titleBar.close')}
            // close() 而非 destroy()：要走 onCloseRequested，AI 会话确认与配置落盘都挂在那
            onClick={() => void getCurrentWindow().close()}
          >
            {ICON_CLOSE}
          </button>
        </div>
      )}
    </div>
  );
}
