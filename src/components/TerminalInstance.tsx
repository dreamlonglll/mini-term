import { useEffect, useRef, useState, useCallback } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useAppStore } from '../store';
import { getOrCreateTerminal, getCachedTerminal, activateWebgl, getTerminalTheme, DARK_TERMINAL_THEME, writePtyInput, copyTerminalSelection, pasteToTerminal, resolveTerminalFontFamily, reloadLigaturesForPty, resetRenderStateForPty } from '../utils/terminalCache';
import { getResolvedTheme } from '../utils/themeManager';
import { showContextMenu, type MenuEntry } from '../utils/contextMenu';
import { isFileDragging, getFileDragPath } from '../utils/fileDragState';
import { useT, t } from '../i18n';
import type { SshConnection } from '../types';
import '@xterm/xterm/css/xterm.css';

interface Props {
  ptyId: number;
}

/**
 * 把 SSH 连接拼成 ssh 命令行。
 * identityPath 为解析后的私钥路径(可能是后端收紧权限的临时副本),
 * 未配置私钥时传 undefined。
 */
function buildSshCommand(conn: SshConnection, identityPath: string | undefined): string {
  const parts = ['ssh'];
  if (conn.port && conn.port !== 22) parts.push('-p', String(conn.port));
  const identity = identityPath?.trim();
  // 反斜杠转正斜杠:Nushell/bash 等会把双引号内的 "\" 当转义符导致报错,
  // 而 Windows OpenSSH 接受正斜杠路径,正斜杠在所有 shell 中都安全
  if (identity) parts.push('-i', `"${identity.replace(/\\/g, '/')}"`);
  parts.push(`${conn.user}@${conn.host}`);
  return parts.join(' ');
}

/** 在指定终端中连接 SSH:有密码先注册自动填充,再写入 ssh 命令并回车 */
async function connectSsh(ptyId: number, conn: SshConnection): Promise<void> {
  if (conn.password) {
    try {
      await invoke('arm_ssh_autofill', { ptyId, password: conn.password });
    } catch {
      // 注册自动填充失败不阻断连接,用户可在终端手动输入密码
    }
  }
  // 配了私钥时先复制到权限收紧的临时副本,绕过 OpenSSH 的
  // "UNPROTECTED PRIVATE KEY FILE" 拒绝;准备失败回退原始路径让 ssh 自行报错
  let identityPath = conn.identityFile?.trim() || undefined;
  if (identityPath) {
    try {
      identityPath = await invoke<string>('prepare_ssh_key', { identityFile: identityPath });
    } catch (e) {
      console.error('准备 SSH 私钥临时副本失败,回退原始路径:', e);
    }
  }
  await writePtyInput(ptyId, `${buildSshCommand(conn, identityPath)}\r`);
  getCachedTerminal(ptyId)?.term.focus();
}

/** 构建终端右键菜单的「SSH 连接」子菜单(按 group 分组) */
function buildSshSubmenu(connections: SshConnection[], ptyId: number): MenuEntry[] {
  const groups: { group?: string; items: SshConnection[] }[] = [];
  for (const conn of connections) {
    const g = conn.group?.trim() || undefined;
    let bucket = groups.find((x) => x.group === g);
    if (!bucket) {
      bucket = { group: g, items: [] };
      groups.push(bucket);
    }
    bucket.items.push(conn);
  }
  const hasNamedGroup = groups.some((g) => g.group);
  const entries: MenuEntry[] = [];
  for (const bucket of groups) {
    if (bucket.group || hasNamedGroup) {
      entries.push({ header: bucket.group ?? t('terminal.ungrouped') });
    }
    for (const conn of bucket.items) {
      entries.push({ label: conn.name, onClick: () => void connectSsh(ptyId, conn) });
    }
  }
  return entries;
}

export function TerminalInstance({ ptyId }: Props) {
  const t = useT();
  const containerRef = useRef<HTMLDivElement>(null);
  const [fileDrag, setFileDrag] = useState(false);
  const terminalFontSize = useAppStore((s) => s.config.terminalFontSize);
  const terminalFontFamily = useAppStore((s) => s.config.terminalFontFamily);
  const terminalLigatures = useAppStore((s) => s.config.terminalLigatures);
  const terminalFollowTheme = useAppStore((s) => s.config.terminalFollowTheme);
  const sshConnections = useAppStore((s) => s.config.sshConnections);

  // 终端不跟随主题且处于浅色模式时，覆写 CSS 变量让整个终端区域（含 .xterm）统一深色
  const forceDarkBg = !terminalFollowTheme && getResolvedTheme() === 'light';
  const termStyle = forceDarkBg
    ? { '--bg-terminal': DARK_TERMINAL_THEME.background } as React.CSSProperties
    : undefined;

  useEffect(() => {
    const container = containerRef.current;
    if (!container) return;

    const { term, fitAddon, wrapper } = getOrCreateTerminal(ptyId);

    container.appendChild(wrapper);

    // fit() 前记住滚动位置（appendChild 不触发 reflow，buffer 状态尚未改变）
    const bufBefore = term.buffer.active;
    const mountWasAtBottom = bufBefore.baseY + term.rows >= bufBefore.length;

    requestAnimationFrame(() => {
      if (container.clientWidth > 0 && container.clientHeight > 0) {
        fitAddon.fit();
        invoke('resize_pty', { ptyId, cols: term.cols, rows: term.rows });
        term.refresh(0, term.rows - 1);
        // split/remount 后视口可能停留在 buffer 顶部，滚回光标位置
        if (mountWasAtBottom) term.scrollToBottom();
        // 等 canvas 渲染器首帧合成上屏后再加载 WebGL，避免替换 canvas 时闪白
        requestAnimationFrame(() => {
          activateWebgl(ptyId);
          // mount 后重建本终端 model/顶点缓冲，对齐共享 atlas 当前布局
          // (只清自己，不动共享 atlas —— 见 resetRenderStateForPty 注释)
          requestAnimationFrame(() => resetRenderStateForPty(ptyId));
        });
      }
    });

    let rafId: number;
    let settleId: ReturnType<typeof setTimeout>;
    // 初始值用挂载前采样值，避免 ResizeObserver 首次回调时 fit 已改变 buffer 状态
    let wasAtBottom = mountWasAtBottom;
    let resizing = false;
    const observer = new ResizeObserver(() => {
      if (!resizing) {
        const buf = term.buffer.active;
        wasAtBottom = buf.baseY + term.rows >= buf.length;
        resizing = true;
      }
      cancelAnimationFrame(rafId);
      rafId = requestAnimationFrame(() => {
        if (container.clientWidth > 0 && container.clientHeight > 0) {
          fitAddon.fit();
        }
      });
      // resize 结束后做一次完整刷新，修复 reflow 残留的空白行/空格
      clearTimeout(settleId);
      settleId = setTimeout(() => {
        resizing = false;
        if (container.clientWidth > 0 && container.clientHeight > 0) {
          fitAddon.fit();
          term.refresh(0, term.rows - 1);
          // split/resize 后若用户原本在底部，确保视口跟随光标
          if (wasAtBottom) term.scrollToBottom();
        }
      }, 150);
    });
    observer.observe(container);

    const visibilityObserver = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        requestAnimationFrame(() => {
          fitAddon.fit();
          term.refresh(0, term.rows - 1);
          // 可见性恢复时重建 model/顶点缓冲，兜底 _isPaused 拦截期间共享 atlas 变更的残留
          resetRenderStateForPty(ptyId);
        });
      }
    });
    visibilityObserver.observe(container);

    return () => {
      cancelAnimationFrame(rafId);
      clearTimeout(settleId);
      observer.disconnect();
      visibilityObserver.disconnect();
      wrapper.remove();
    };
  }, [ptyId]);

  useEffect(() => {
    const cached = getCachedTerminal(ptyId);
    if (cached && terminalFontSize) {
      cached.term.options.fontSize = terminalFontSize;
      cached.fitAddon.fit();
    }
  }, [terminalFontSize, ptyId]);

  useEffect(() => {
    const cached = getCachedTerminal(ptyId);
    if (!cached) return;
    cached.term.options.fontFamily = resolveTerminalFontFamily(terminalFontFamily);
    cached.fitAddon.fit();
  }, [terminalFontFamily, ptyId]);

  // ligatures 开关切换 / 字体切换 → 重做 ligatures + WebGL atlas
  // (上游 #5455:font-feature-settings 变更不会自动进 WebGL 纹理 atlas,需 dispose+reload)
  useEffect(() => {
    reloadLigaturesForPty(ptyId);
  }, [terminalLigatures, terminalFontFamily, ptyId]);

  useEffect(() => {
    const handler = () => {
      const cached = getCachedTerminal(ptyId);
      if (cached) {
        const { config } = useAppStore.getState();
        cached.term.options.theme = getTerminalTheme(config.terminalFollowTheme ?? true);
      }
    };
    window.addEventListener('theme-changed', handler);
    return () => window.removeEventListener('theme-changed', handler);
  }, [ptyId]);

  // 外部拖拽（资源管理器 → 终端）：监听 useExternalFileDrop 派发的自定义事件
  useEffect(() => {
    const handler = (e: Event) => {
      setFileDrag((e as CustomEvent<number | null>).detail === ptyId);
    };
    window.addEventListener('external-file-drag', handler);
    return () => window.removeEventListener('external-file-drag', handler);
  }, [ptyId]);

  // 内部拖拽（FileTree → 终端）：自定义鼠标事件，规避 WebView2 dragDropEnabled 拦截
  const dropZoneRef = useRef<HTMLDivElement>(null);

  const handleMouseMove = useCallback(() => {
    if (isFileDragging() && !fileDrag) setFileDrag(true);
  }, [fileDrag]);

  const handleMouseLeave = useCallback(() => {
    if (fileDrag) setFileDrag(false);
  }, [fileDrag]);

  const handleMouseUp = useCallback(() => {
    const path = getFileDragPath();
    if (path) {
      setFileDrag(false);
      void writePtyInput(ptyId, `'${path}'`);
      getCachedTerminal(ptyId)?.term.focus();
    }
  }, [ptyId]);

  const handleContextMenu = (e: React.MouseEvent<HTMLDivElement>) => {
    e.preventDefault();
    const hasSelection = !!getCachedTerminal(ptyId)?.term.getSelection();
    const menu: MenuEntry[] = [
      {
        label: t('terminal.copy'),
        disabled: !hasSelection,
        onClick: () => { void copyTerminalSelection(ptyId); },
      },
      {
        label: t('terminal.paste'),
        onClick: () => {
          void pasteToTerminal(ptyId);
          getCachedTerminal(ptyId)?.term.focus();
        },
      },
      { separator: true },
      sshConnections.length > 0
        ? { label: t('terminal.sshConnect'), submenu: buildSshSubmenu(sshConnections, ptyId) }
        : { label: t('terminal.sshConnectEmpty'), disabled: true },
    ];
    showContextMenu(e.clientX, e.clientY, menu);
  };

  return (
    <div className="w-full h-full flex flex-col">
      <div
        ref={dropZoneRef}
        className="flex-1 relative bg-[var(--bg-terminal)]"
        style={termStyle}
        data-terminal-drop
        data-pty-id={ptyId}
        onMouseMove={handleMouseMove}
        onMouseLeave={handleMouseLeave}
        onMouseUp={handleMouseUp}
        onContextMenu={handleContextMenu}
      >
        {/* cursor-none 曾挂在这里,但 xterm.css 的 `.xterm { cursor: text }` 把它整个盖掉了,
            只在 padding 缝隙生效 —— 留着纯属误导,已移除 */}
        <div ref={containerRef} className="absolute top-1.5 bottom-0 left-2.5 right-0" />

        {fileDrag && (
          <div
            className="absolute inset-1 z-10 flex items-center justify-center pointer-events-none rounded-[var(--radius-md)]"
            style={{ background: 'var(--accent-subtle)', border: '2px dashed var(--accent)' }}
          >
            <span className="text-[var(--accent)] text-xs px-3 py-1.5 rounded-[var(--radius-md)]"
              style={{ background: 'var(--bg-overlay)' }}>
              {t('terminal.dropToInsertPath')}
            </span>
          </div>
        )}
      </div>
    </div>
  );
}
