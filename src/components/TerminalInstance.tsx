import { useEffect, useRef } from 'react';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import type { PtyOutputPayload } from '../types';
import '@xterm/xterm/css/xterm.css';

interface Props {
  ptyId: number;
}

export function TerminalInstance({ ptyId }: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitAddonRef = useRef<FitAddon | null>(null);

  useEffect(() => {
    if (!containerRef.current) return;

    const term = new Terminal({
      fontSize: 14,
      fontFamily: 'Cascadia Code, Consolas, monospace',
      cursorBlink: true,
      cursorStyle: 'block',
      scrollback: 5000,
      theme: {
        background: '#0d0d1a',
        foreground: '#d4d4d4',
        cursor: '#ffffff',
        selectionBackground: '#7c83ff44',
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
      ref={containerRef}
      className="w-full h-full"
      onDrop={(e) => {
        e.preventDefault();
        const filePath = e.dataTransfer.getData('text/plain');
        if (filePath) {
          invoke('write_pty', { ptyId, data: filePath });
        }
      }}
      onDragOver={(e) => e.preventDefault()}
    />
  );
}
