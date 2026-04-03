/**
 * 终端实例缓存：在 React 组件卸载/重新挂载期间保持 xterm.js Terminal 存活。
 *
 * 问题：分屏操作导致 SplitLayout 从 leaf 变为 split 节点，React 会卸载旧的
 * TerminalInstance 并重建新的，xterm.js 实例被 dispose，终端内容丢失。
 *
 * 方案：Terminal 实例按 ptyId 缓存。组件 mount 时附着 wrapper 到容器，
 * unmount 时仅分离 wrapper，不销毁 Terminal。仅在面板真正关闭时调用 dispose。
 */

import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { readText, writeText } from '@tauri-apps/plugin-clipboard-manager';
import { useAppStore } from '../store';
import type { PtyOutputPayload } from '../types';

export interface CachedTerminal {
  term: Terminal;
  fitAddon: FitAddon;
  wrapper: HTMLDivElement;
}

interface CachedEntry extends CachedTerminal {
  cleanup: () => void;
}

const cache = new Map<number, CachedEntry>();

export function getOrCreateTerminal(ptyId: number): CachedTerminal {
  const existing = cache.get(ptyId);
  if (existing) return existing;

  // 创建 wrapper 容器，xterm.js 会在其中渲染
  const wrapper = document.createElement('div');
  wrapper.style.width = '100%';
  wrapper.style.height = '100%';

  const term = new Terminal({
    fontSize: useAppStore.getState().config.terminalFontSize ?? 14,
    fontFamily: "'JetBrains Mono', 'Cascadia Code', Consolas, 'Sarasa Term SC', monospace",
    fontWeight: '400',
    fontWeightBold: '600',
    cursorBlink: true,
    cursorStyle: 'bar',
    cursorWidth: 2,
    scrollback: 100000,
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
  term.open(wrapper);

  // WebGL 渲染，降级时回退到 DOM
  try {
    const webgl = new WebglAddon();
    webgl.onContextLoss(() => {
      webgl.dispose();
      term.refresh(0, term.rows - 1);
    });
    term.loadAddon(webgl);
  } catch {
    // WebGL 不支持，回退到 DOM
  }

  // 剪贴板快捷键
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

  // 用户输入 → PTY
  const onDataDisp = term.onData((data) => {
    term.scrollToBottom();
    invoke('write_pty', { ptyId, data });
  });

  // 终端 resize → 同步到 PTY
  const onResizeDisp = term.onResize(({ cols, rows }) => {
    invoke('resize_pty', { ptyId, cols, rows });
  });

  // PTY 输出 → 终端
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

  const cleanup = () => {
    cancelled = true;
    unlisten?.();
    onDataDisp.dispose();
    onResizeDisp.dispose();
    term.dispose();
  };

  const entry: CachedEntry = { term, fitAddon, wrapper, cleanup };
  cache.set(ptyId, entry);
  return entry;
}

/** 获取已缓存的终端（不创建新的） */
export function getCachedTerminal(ptyId: number): CachedTerminal | undefined {
  return cache.get(ptyId);
}

/** 彻底销毁终端（面板关闭 / kill_pty 后调用） */
export function disposeTerminal(ptyId: number): void {
  const entry = cache.get(ptyId);
  if (!entry) return;
  entry.wrapper.remove();
  entry.cleanup();
  cache.delete(ptyId);
}
