import { useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { readText, writeText } from '@tauri-apps/plugin-clipboard-manager';
import { useAppStore } from '../store';
import type { PtyOutputPayload, PaneStatus } from '../types';
import { StatusDot } from './StatusDot';
import { getDraggingTabId } from '../utils/dragState';
import '@xterm/xterm/css/xterm.css';

type DropZone = 'top' | 'bottom' | 'left' | 'right';

function getDropZone(rect: DOMRect, clientX: number, clientY: number): DropZone {
  const x = (clientX - rect.left) / rect.width;
  const y = (clientY - rect.top) / rect.height;
  const aboveMain = y < x;
  const aboveAnti = y < 1 - x;
  if (aboveMain && aboveAnti) return 'top';
  if (!aboveMain && !aboveAnti) return 'bottom';
  if (!aboveMain && aboveAnti) return 'left';
  return 'right';
}

const dropZoneOverlay: Record<DropZone, React.CSSProperties> = {
  top: { top: 0, left: 0, right: 0, height: '50%' },
  bottom: { bottom: 0, left: 0, right: 0, height: '50%' },
  left: { top: 0, left: 0, bottom: 0, width: '50%' },
  right: { top: 0, right: 0, bottom: 0, width: '50%' },
};

interface Props {
  ptyId: number;
  paneId?: string;
  shellName?: string;
  status?: PaneStatus;
  onSplit?: (paneId: string, direction: 'horizontal' | 'vertical') => void;
  onClose?: (paneId: string) => void;
  onTabDrop?: (sourceTabId: string, targetPaneId: string, direction: 'horizontal' | 'vertical', position: 'before' | 'after') => void;
}

export function TerminalInstance({ ptyId, paneId, shellName, status, onSplit, onClose, onTabDrop }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  const [dragOver, setDragOver] = useState(false);
  const [tabDropZone, setTabDropZone] = useState<DropZone | null>(null);
  const terminalFontSize = useAppStore((s) => s.config.terminalFontSize);

  useEffect(() => {
    if (!containerRef.current) return;

    const term = new Terminal({
      fontSize: useAppStore.getState().config.terminalFontSize ?? 14,
      fontFamily: "'JetBrains Mono', 'Cascadia Code', Consolas, monospace",
      fontWeight: '400',
      fontWeightBold: '600',
      cursorBlink: true,
      cursorStyle: 'bar',
      cursorWidth: 2,
      scrollback: 5000,
      letterSpacing: 0,
      lineHeight: 1.35,
      theme: {
        background: '#100f0d',
        foreground: '#d8d4cc',
        cursor: '#c8805a',
        cursorAccent: '#100f0d',
        selectionBackground: '#c8805a30',
        selectionForeground: '#e5e0d8',
        black: '#2a2824',
        red: '#d4605a',
        green: '#6bb87a',
        yellow: '#d4a84a',
        blue: '#6896c8',
        magenta: '#b08cd4',
        cyan: '#7dcfb8',
        white: '#d8d4cc',
        brightBlack: '#5c5850',
        brightRed: '#e07060',
        brightGreen: '#80d090',
        brightYellow: '#e0b860',
        brightBlue: '#80aad8',
        brightMagenta: '#c0a0e0',
        brightCyan: '#90e0c8',
        brightWhite: '#e5e0d8',
      },
    });

    const fitAddon = new FitAddon();
    term.loadAddon(fitAddon);
    term.open(containerRef.current);

    try {
      const webgl = new WebglAddon();
      webgl.onContextLoss(() => webgl.dispose());
      term.loadAddon(webgl);
    } catch {
      // WebGL 不支持时回退到 Canvas
    }

    fitAddon.fit();
    termRef.current = term;
    fitAddonRef.current = fitAddon;

    // Ctrl+Shift+C 复制 / Ctrl+Shift+V 粘贴
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== 'keydown') return true;
      if (e.ctrlKey && e.shiftKey && e.code === 'KeyC') {
        e.preventDefault();
        const sel = term.getSelection();
        if (sel) writeText(sel);
        return false;
      }
      if (e.ctrlKey && e.shiftKey && e.code === 'KeyV') {
        e.preventDefault();
        readText().then((text) => {
          if (text) invoke('write_pty', { ptyId, data: text });
        });
        return false;
      }
      return true;
    });

    invoke('resize_pty', { ptyId, cols: term.cols, rows: term.rows });

    const onDataDisposable = term.onData((data) => {
      term.scrollToBottom();
      invoke('write_pty', { ptyId, data });
    });

    let cancelled = false;
    let unlisten: (() => void) | undefined;
    listen<PtyOutputPayload>('pty-output', (event) => {
      if (event.payload.ptyId === ptyId) {
        term.write(event.payload.data);
      }
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });

    const onResizeDisposable = term.onResize(({ cols, rows }) => {
      invoke('resize_pty', { ptyId, cols, rows });
    });

    let rafId: number;
    const observer = new ResizeObserver(() => {
      cancelAnimationFrame(rafId);
      rafId = requestAnimationFrame(() => fitAddon.fit());
    });
    observer.observe(containerRef.current);

    // 检测 display:none→block 可见性变化，触发 fit 重算尺寸
    const visibilityObserver = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        requestAnimationFrame(() => fitAddon.fit());
      }
    });
    visibilityObserver.observe(containerRef.current);

    return () => {
      cancelled = true;
      unlisten?.();
      cancelAnimationFrame(rafId);
      observer.disconnect();
      visibilityObserver.disconnect();
      onDataDisposable.dispose();
      onResizeDisposable.dispose();
      term.dispose();
    };
  }, [ptyId]);

  // 动态更新终端字体大小
  useEffect(() => {
    const term = termRef.current;
    const fitAddon = fitAddonRef.current;
    if (term && terminalFontSize) {
      term.options.fontSize = terminalFontSize;
      fitAddon?.fit();
    }
  }, [terminalFontSize]);

  const isTabDrag = (e: React.DragEvent) => e.dataTransfer.types.includes('application/tab-id');

  return (
    <div className="w-full h-full flex flex-col">
      {/* 面板标题栏 */}
      <div
        className="flex items-center gap-1.5 px-2 py-[3px] bg-[var(--bg-elevated)] border-b border-[var(--border-subtle)] text-[10px] select-none shrink-0"
        style={{ WebkitAppRegion: 'no-drag' } as React.CSSProperties}
      >
        {status && <StatusDot status={status} />}
        <span className="text-[var(--text-secondary)] font-medium truncate flex-1">
          {shellName ?? 'Terminal'}
        </span>
        {paneId && onSplit && (
          <>
            <span
              className="text-[var(--text-muted)] hover:text-[var(--accent)] cursor-pointer transition-colors px-0.5"
              title="向右分屏"
              onClick={() => onSplit(paneId, 'horizontal')}
            >
              ┃
            </span>
            <span
              className="text-[var(--text-muted)] hover:text-[var(--accent)] cursor-pointer transition-colors px-0.5"
              title="向下分屏"
              onClick={() => onSplit(paneId, 'vertical')}
            >
              ━
            </span>
          </>
        )}
        {paneId && onClose && (
          <span
            className="text-[var(--text-muted)] hover:text-[var(--color-error)] cursor-pointer text-[9px] transition-colors pl-1"
            title="关闭面板"
            onClick={() => onClose(paneId)}
          >
            ✕
          </span>
        )}
      </div>

      {/* 终端内容区 */}
      <div
        className="flex-1 relative bg-[#100f0d]"
        onDragEnter={(e) => {
          e.preventDefault();
          if (isTabDrag(e)) {
            const rect = e.currentTarget.getBoundingClientRect();
            setTabDropZone(getDropZone(rect, e.clientX, e.clientY));
          } else {
            setDragOver(true);
          }
        }}
        onDragOver={(e) => {
          e.preventDefault();
          if (isTabDrag(e)) {
            e.dataTransfer.dropEffect = 'move';
            const rect = e.currentTarget.getBoundingClientRect();
            setTabDropZone(getDropZone(rect, e.clientX, e.clientY));
          } else {
            e.dataTransfer.dropEffect = 'copy';
          }
        }}
        onDragLeave={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node)) {
            setDragOver(false);
            setTabDropZone(null);
          }
        }}
        onDrop={(e) => {
          e.preventDefault();
          setDragOver(false);
          setTabDropZone(null);

          const tabId = getDraggingTabId();
          if (tabId && paneId && onTabDrop) {
            const rect = e.currentTarget.getBoundingClientRect();
            const zone = getDropZone(rect, e.clientX, e.clientY);
            const direction: 'horizontal' | 'vertical' =
              zone === 'left' || zone === 'right' ? 'horizontal' : 'vertical';
            const position: 'before' | 'after' =
              zone === 'left' || zone === 'top' ? 'before' : 'after';
            onTabDrop(tabId, paneId, direction, position);
            return;
          }

          const filePath = e.dataTransfer.getData('text/plain');
          if (filePath) {
            invoke('write_pty', { ptyId, data: filePath });
          }
        }}
      >
        {/* xterm.js 渲染容器 */}
        <div ref={containerRef} className="absolute top-1.5 bottom-0 left-2.5 right-0" />

        {/* 文件拖拽视觉提示 */}
        {dragOver && (
          <div
            className="absolute inset-1 z-10 flex items-center justify-center pointer-events-none rounded-[var(--radius-md)]"
            style={{ background: 'rgba(200, 128, 90, 0.06)', border: '2px dashed var(--accent)' }}
          >
            <span className="text-[var(--accent)] text-xs px-3 py-1.5 rounded-[var(--radius-md)]"
              style={{ background: 'var(--bg-overlay)' }}>
              释放以插入路径
            </span>
          </div>
        )}

        {/* Tab 拖拽分屏方向指示 */}
        {tabDropZone && (
          <div
            className="absolute z-10 pointer-events-none"
            style={{
              ...dropZoneOverlay[tabDropZone],
              background: 'rgba(200, 128, 90, 0.12)',
              borderRadius: '4px',
            }}
          />
        )}
      </div>
    </div>
  );
}
