import { useEffect, useRef, useState } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { PtyOutputPayload } from '../types';
import '@xterm/xterm/css/xterm.css';

interface Props {
  ptyId: number;
  paneId?: string;
  onSplit?: (paneId: string, direction: 'horizontal' | 'vertical') => void;
  onClose?: (paneId: string) => void;
}

export function TerminalInstance({ ptyId, paneId, onSplit, onClose }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const wrapperRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);
  const [dragOver, setDragOver] = useState(false);

  useEffect(() => {
    if (!containerRef.current) return;

    const term = new Terminal({
      fontSize: 13,
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

    // WebGL 渲染加速
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

    // 通知 Rust 终端尺寸
    invoke('resize_pty', { ptyId, cols: term.cols, rows: term.rows });

    // 用户输入 -> Rust PTY
    const onDataDisposable = term.onData((data) => {
      invoke('write_pty', { ptyId, data });
    });

    // Rust PTY 输出 -> xterm
    let unlisten: (() => void) | undefined;
    listen<PtyOutputPayload>('pty-output', (event) => {
      if (event.payload.ptyId === ptyId) {
        term.write(event.payload.data);
      }
    }).then((fn) => {
      unlisten = fn;
    });

    // 终端尺寸变化
    const onResizeDisposable = term.onResize(({ cols, rows }) => {
      invoke('resize_pty', { ptyId, cols, rows });
    });

    // 容器尺寸变化时 fit
    const observer = new ResizeObserver(() => {
      fitAddon.fit();
    });
    observer.observe(containerRef.current);

    return () => {
      observer.disconnect();
      onDataDisposable.dispose();
      onResizeDisposable.dispose();
      unlisten?.();
      term.dispose();
    };
  }, [ptyId]);

  return (
    <div
      ref={wrapperRef}
      className="w-full h-full relative"
      onDragEnter={(e) => {
        e.preventDefault();
        setDragOver(true);
      }}
    >
      {/* xterm.js 渲染容器 */}
      <div ref={containerRef} className="absolute inset-0" />

      {/* 拖拽放置覆盖层 — 位于 xterm canvas 之上，仅在拖拽时激活 */}
      {dragOver && (
        <div
          className="absolute inset-0 z-10 flex items-center justify-center"
          style={{ background: 'rgba(200, 128, 90, 0.08)', border: '2px dashed var(--accent)', borderRadius: 'var(--radius-md)' }}
          onDragOver={(e) => e.preventDefault()}
          onDragLeave={(e) => {
            // 仅当离开 wrapper 时才取消
            if (wrapperRef.current && !wrapperRef.current.contains(e.relatedTarget as Node)) {
              setDragOver(false);
            }
          }}
          onDrop={(e) => {
            e.preventDefault();
            setDragOver(false);
            const filePath = e.dataTransfer.getData('text/plain');
            if (filePath) {
              invoke('write_pty', { ptyId, data: filePath });
            }
          }}
        >
          <span className="text-[var(--accent)] text-xs px-3 py-1.5 rounded-[var(--radius-md)]"
            style={{ background: 'var(--bg-overlay)' }}>
            释放以插入路径
          </span>
        </div>
      )}

      {/* 右键菜单绑定在 wrapper 上 */}
      <div className="absolute inset-0" style={{ pointerEvents: dragOver ? 'none' : 'auto', zIndex: dragOver ? -1 : 'auto' }}
      onContextMenu={(e) => {
        e.preventDefault();
        if (!paneId || !onSplit) return;

        const menu = document.createElement('div');
        menu.className = 'fixed ctx-menu text-xs';
        menu.style.left = `${e.clientX}px`;
        menu.style.top = `${e.clientY}px`;

        const items = [
          { label: '向右分屏', dir: 'horizontal' as const },
          { label: '向下分屏', dir: 'vertical' as const },
        ];

        items.forEach(({ label, dir }) => {
          const item = document.createElement('div');
          item.className = 'ctx-menu-item';
          item.textContent = label;
          item.onclick = () => {
            onSplit(paneId, dir);
            menu.remove();
          };
          menu.appendChild(item);
        });

        const sep = document.createElement('div');
        sep.className = 'ctx-menu-sep';
        menu.appendChild(sep);

        const closeItem = document.createElement('div');
        closeItem.className = 'ctx-menu-item danger';
        closeItem.textContent = '关闭面板';
        closeItem.onclick = () => {
          if (onClose) onClose(paneId);
          menu.remove();
        };
        menu.appendChild(closeItem);

        document.body.appendChild(menu);
        const dismiss = () => { menu.remove(); document.removeEventListener('click', dismiss); };
        setTimeout(() => document.addEventListener('click', dismiss), 0);
      }}
      />
    </div>
  );
}
