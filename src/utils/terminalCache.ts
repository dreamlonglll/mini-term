/**
 * 终端实例缓存：在 React 组件卸载/重新挂载期间保持 xterm.js Terminal 存活。
 *
 * 问题：分屏操作导致 SplitLayout 从 leaf 变为 split 节点，React 会卸载旧的
 * TerminalInstance 并重建新的，xterm.js 实例被 dispose，终端内容丢失。
 *
 * 方案：Terminal 实例按 ptyId 缓存。组件 mount 时附着 wrapper 到容器，
 * unmount 时仅分离 wrapper，不销毁 Terminal。仅在面板真正关闭时调用 dispose。
 */

import { Terminal, type IMarker, type IDecoration } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import { WebglAddon } from '@xterm/addon-webgl';
import { LigaturesAddon } from '@xterm/addon-ligatures';
import type { SearchAddon } from '@xterm/addon-search';
import { activateUnicodeWidth } from './terminalUnicodeWidth';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { readText, readImage, writeText } from '@tauri-apps/plugin-clipboard-manager';
import { useAppStore } from '../store';
import type { PtyOutputPayload } from '../types';
import { getResolvedTheme } from './themeManager';
import { createPtyWriteQueue } from './ptyWriteQueue';
import { getCurrentLineSnapshotFromBuffer } from './terminalSnapshot';
import { resolvePasteTarget, mapPastedFilePath, type PasteTarget } from './pastePath';
import { t } from '../i18n';

export interface CachedTerminal {
  term: Terminal;
  fitAddon: FitAddon;
  wrapper: HTMLDivElement;
}

export const DEFAULT_TERMINAL_FONT_FAMILY =
  "'JetBrainsMono Nerd Font', 'CaskaydiaCove Nerd Font', 'JetBrains Mono', 'Cascadia Code', Consolas, monospace";

export function resolveTerminalFontFamily(value: string | undefined): string {
  const trimmed = value?.trim();
  return trimmed ? trimmed : DEFAULT_TERMINAL_FONT_FAMILY;
}

interface CachedEntry extends CachedTerminal {
  cleanup: () => void;
  webglLoaded: boolean;
  webglAddon?: WebglAddon;
  ligaturesAddon?: LigaturesAddon;
  /** 终端内查找的 addon，首次 Ctrl+F 时由 terminalSearch.ts 懒加载。
   *  不需要单独回收：term.dispose() 会连带 dispose 所有已加载 addon。 */
  searchAddon?: SearchAddon;
}

export const DARK_TERMINAL_THEME = {
  background: '#0a0908',
  foreground: '#d8d4cc',
  cursor: '#c8805a',
  cursorAccent: '#0a0908',
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
};

export const LIGHT_TERMINAL_THEME = {
  background: '#fafafa',
  foreground: '#1a1a1a',
  cursor: '#b06830',
  cursorAccent: '#fafafa',
  selectionBackground: '#b0683030',
  selectionForeground: '#1a1a1a',
  black: '#1a1a1a',
  red: '#c0392b',
  green: '#2d8a46',
  yellow: '#b08620',
  blue: '#2860a0',
  magenta: '#8a5cb8',
  cyan: '#1a8a6a',
  white: '#808080',
  brightBlack: '#666666',
  brightRed: '#e04030',
  brightGreen: '#38a058',
  brightYellow: '#c89830',
  brightBlue: '#3870b8',
  brightMagenta: '#a070d0',
  brightCyan: '#28a080',
  brightWhite: '#a0a0a0',
};

export const BLUEPRINT_TERMINAL_THEME = {
  background: '#060e1c',
  foreground: '#d9e2ec',
  cursor: '#22d3ee',
  cursorAccent: '#060e1c',
  selectionBackground: 'rgba(34,211,238,0.2)',
  selectionForeground: '#f8fafc',
  black: '#0a1628',
  red: '#ef4444',
  green: '#22c55e',
  yellow: '#f97316',
  blue: '#60a5fa',
  magenta: '#a78bfa',
  cyan: '#22d3ee',
  white: '#e2e8f0',
  brightBlack: '#1a365d',
  brightRed: '#f87171',
  brightGreen: '#4ade80',
  brightYellow: '#fb923c',
  brightBlue: '#93c5fd',
  brightMagenta: '#c4b5fd',
  brightCyan: '#67e8f9',
  brightWhite: '#f8fafc',
};

export const BLUEPRINT_LIGHT_TERMINAL_THEME = {
  background: '#f5f8fb',
  foreground: '#0f172a',
  cursor: '#0e7490',
  cursorAccent: '#f5f8fb',
  selectionBackground: 'rgba(14,116,144,0.15)',
  selectionForeground: '#0f172a',
  black: '#1e293b',
  red: '#dc2626',
  green: '#15803d',
  yellow: '#c2410c',
  blue: '#1d4ed8',
  magenta: '#7c3aed',
  cyan: '#0e7490',
  white: '#94a3b8',
  brightBlack: '#475569',
  brightRed: '#ef4444',
  brightGreen: '#22c55e',
  brightYellow: '#f97316',
  brightBlue: '#3b82f6',
  brightMagenta: '#8b5cf6',
  brightCyan: '#14b8a6',
  brightWhite: '#64748b',
};

export const FLUENT2_TERMINAL_THEME = {
  background: '#15181f',
  foreground: '#e8e8e8',
  cursor: '#4cc2ff',
  cursorAccent: '#15181f',
  selectionBackground: 'rgba(76,194,255,0.22)',
  selectionForeground: '#ffffff',
  black: '#1f1f1f',
  red: '#f87171',
  green: '#6ccb5f',
  yellow: '#fce100',
  blue: '#4cc2ff',
  magenta: '#c8a2ff',
  cyan: '#61d6d6',
  white: '#e8e8e8',
  brightBlack: '#767676',
  brightRed: '#ff9594',
  brightGreen: '#80e16e',
  brightYellow: '#ffe555',
  brightBlue: '#6fcdff',
  brightMagenta: '#d3b4ff',
  brightCyan: '#88e0e0',
  brightWhite: '#ffffff',
};

export const FLUENT2_LIGHT_TERMINAL_THEME = {
  background: '#fafbfd',
  foreground: '#1a1a1a',
  cursor: '#0067c0',
  cursorAccent: '#fafbfd',
  selectionBackground: 'rgba(0,103,192,0.18)',
  selectionForeground: '#1a1a1a',
  black: '#1a1a1a',
  red: '#c42b1c',
  green: '#107c10',
  yellow: '#b89500',
  blue: '#0067c0',
  magenta: '#8764b8',
  cyan: '#038387',
  white: '#767676',
  brightBlack: '#4a4a4a',
  brightRed: '#d13438',
  brightGreen: '#13a10e',
  brightYellow: '#c19c00',
  brightBlue: '#3b9eff',
  brightMagenta: '#b146c2',
  brightCyan: '#3a96dd',
  brightWhite: '#ffffff',
};

export function getTerminalTheme(terminalFollowTheme: boolean): typeof DARK_TERMINAL_THEME {
  if (!terminalFollowTheme) return DARK_TERMINAL_THEME;
  const skin = useAppStore.getState().config.skin;
  if (skin === 'blueprint') {
    return getResolvedTheme() === 'light'
      ? BLUEPRINT_LIGHT_TERMINAL_THEME
      : BLUEPRINT_TERMINAL_THEME;
  }
  if (skin === 'fluent2') {
    return getResolvedTheme() === 'light'
      ? FLUENT2_LIGHT_TERMINAL_THEME
      : FLUENT2_TERMINAL_THEME;
  }
  if (getResolvedTheme() === 'light') return LIGHT_TERMINAL_THEME;
  return DARK_TERMINAL_THEME;
}

const cache = new Map<number, CachedEntry>();

const aiPtyIds = new Set<number>();

export function markAiPty(ptyId: number, isAi: boolean) {
  if (isAi) aiPtyIds.add(ptyId);
  else aiPtyIds.delete(ptyId);
}

export function isAiPty(ptyId: number): boolean {
  return aiPtyIds.has(ptyId);
}

let globalPtyListenerInit = false;
function ensureGlobalPtyOutputListener() {
  if (globalPtyListenerInit) return;
  globalPtyListenerInit = true;
  listen<PtyOutputPayload>('pty-output', (event) => {
    const entry = cache.get(event.payload.ptyId);
    if (entry) {
      entry.term.write(event.payload.data);
    }
  });
}
const enqueuePtyWrite = createPtyWriteQueue((ptyId, data, lineSnapshot) => {
  const payload = lineSnapshot === undefined
    ? { ptyId, data }
    : { ptyId, data, lineSnapshot };
  return invoke('write_pty', payload);
});

const markerInstancesByPty = new Map<number, Map<number, IMarker>>();

const FLASH_DECORATION_CSS_BG = 'rgba(245, 197, 24, 0.33)';
const FLASH_DURATION_MS = 300;

/** xterm.js 在 sendFocus 模式下对 focus/blur 发出的 CSI 序列 */
const FOCUS_IN_SEQ = '\x1b[I';
const FOCUS_OUT_SEQ = '\x1b[O';

function isStandaloneEnter(data: string): boolean {
  return data === '\r' || data === '\n' || data === '\r\n';
}

function getCurrentLineSnapshot(term: Terminal): string | undefined {
  const buffer = term.buffer.active;
  const cursorLine = buffer.baseY + buffer.cursorY;
  return getCurrentLineSnapshotFromBuffer(buffer, cursorLine, buffer.cursorX);
}

export function getOrCreateTerminal(ptyId: number): CachedTerminal {
  const existing = cache.get(ptyId);
  if (existing) return existing;

  // 创建 wrapper 容器，xterm.js 会在其中渲染
  const wrapper = document.createElement('div');
  wrapper.style.width = '100%';
  wrapper.style.height = '100%';

  const theme = getTerminalTheme(useAppStore.getState().config.terminalFollowTheme ?? true);
  // 预设背景色，防止首帧渲染前闪屏；始终跟随系统主题 CSS 变量
  wrapper.style.backgroundColor = 'var(--bg-terminal)';

  const term = new Terminal({
    fontSize: useAppStore.getState().config.terminalFontSize ?? 14,
    fontFamily: resolveTerminalFontFamily(useAppStore.getState().config.terminalFontFamily),
    fontWeight: '400',
    fontWeightBold: '600',
    cursorBlink: true,
    cursorStyle: 'bar',
    cursorWidth: 2,
    scrollback: 100000,
    letterSpacing: 0,
    lineHeight: 1.35,
    theme,
    // 前景/背景对比度不足时自动提亮前景（WCAG AA）。Claude Code 的 AskUserQuestion
    // 提问行用近黑前景，在暗色主题下与背景几乎同色，不选中看不见；VS Code 同为 4.5。
    minimumContrastRatio: 4.5,
    // LigaturesAddon 内部用 registerCharacterJoiner（xterm.js proposed API），
    // 不开启 allowProposedApi 加载 addon 会抛 "You must set the allowProposedApi option to true"。
    allowProposedApi: true,
  });

  const fitAddon = new FitAddon();
  term.loadAddon(fitAddon);

  // 切换到带 VS16 修正的 Unicode 11 宽度表:默认 v6 把 ✅❌⚠️ 等 emoji 判为窄字符(width 1),
  // 与生成 box-drawing 表格的 CLI/AI 工具(按 emoji = 2 格对齐)口径不一致导致竖线列错位。
  // 详见 terminalUnicodeWidth.ts。宽度表属 buffer 层,与 WebGL/Ligatures 渲染正交。
  activateUnicodeWidth(term);

  term.open(wrapper);

  // 这里曾拦截 alternate screen 切换(DECSET/DECRST 47/1047/1049),把 TUI 输出摁在
  // 主缓冲区以保住 scrollback。前提"TUI 的清屏/重绘仅影响可视区域"只在一帧不高于
  // 窗口时成立,而 AI TUI 的界面经常比窗口高:
  //   Ink(Claude Code)每帧靠相对光标上移回到帧首再擦除(ESC[nA + ESC[J);
  //   帧高 > rows 时上移被 clamp 在 viewport 顶部,够不到已滚进 scrollback 的那几行
  //   → 擦不干净 → 重画 → 每帧都往 scrollback 落一段残留,越滚越多。
  // tests/tuiScrollback.test.cjs 有 rows=4/帧高=6 的复现:4 帧画完留下 4 份帧头。
  //
  // 用户滚动时尤其明显:AI TUI 都开鼠标追踪,xterm 把 wheel 上报给 TUI 而不滚视口
  // (Terminal._bindMouse 的 bit 16,同时 Viewport 置 handleMouseWheel:false),
  // 于是每滚一次就触发一次额外整屏重绘,当场再灌一份残留。
  //
  // 现已放行:TUI 回到备用缓冲区原地重绘,重复消失,也是 Windows Terminal / iTerm
  // 的标准行为。代价是 AI 运行期间没有 xterm scrollback,滚动由 TUI 自己承担
  // (它本来就收得到滚轮);打点跳转的连带影响见 registerAiMarker。

  // 剪贴板快捷键
  term.attachCustomKeyEventHandler((e) => {
    if (e.type !== 'keydown') return true;
    const mod = e.ctrlKey || e.metaKey;
    // Ctrl+Shift+C / Ctrl+Shift+V：始终生效
    if (mod && e.shiftKey && e.code === 'KeyC') {
      e.preventDefault();
      void copyTerminalSelection(ptyId);
      return false;
    }
    if (mod && e.shiftKey && e.code === 'KeyV') {
      e.preventDefault();
      void pasteToTerminal(ptyId);
      return false;
    }
    // 智能 Ctrl+C/V（设置开启时）：Ctrl+C 有选区则复制并清除选区、
    // 无选区则透传 SIGINT；Ctrl+V 直接粘贴
    if (mod && !e.shiftKey && !e.altKey && useAppStore.getState().config.smartCopyPaste) {
      if (e.code === 'KeyC') {
        if (term.hasSelection()) {
          e.preventDefault();
          void copyTerminalSelection(ptyId);
          term.clearSelection();
          return false;
        }
        return true;
      }
      if (e.code === 'KeyV') {
        e.preventDefault();
        void pasteToTerminal(ptyId);
        return false;
      }
    }
    return true;
  });

  // 用户输入 → PTY
  // 注意:xterm.js 在 TUI 开启 sendFocus(DEC 1004) 模式后,会把 focus/blur
  // 事件也通过 triggerDataEvent 发出 CSI I/CSI O。这不是用户按键,如果也跟着
  // scrollToBottom,用户往上翻历史时一切焦点(点别处或切回来)就会被打回底部。
  const onDataDisp = term.onData((data) => {
    const lineSnapshot = isStandaloneEnter(data)
      ? getCurrentLineSnapshot(term)
      : undefined;
    if (data !== FOCUS_IN_SEQ && data !== FOCUS_OUT_SEQ) {
      term.scrollToBottom();
    }
    void enqueuePtyWrite(ptyId, data, lineSnapshot);
  });

  // 终端 resize → 同步到 PTY
  const onResizeDisp = term.onResize(({ cols, rows }) => {
    invoke('resize_pty', { ptyId, cols, rows });
  });

  // PTY 输出由全局单一监听器分发（避免 N 个终端各自监听导致的 O(N) 事件广播开销）
  ensureGlobalPtyOutputListener();

  const cleanup = () => {
    onDataDisp.dispose();
    onResizeDisp.dispose();
    term.dispose();
  };

  const entry: CachedEntry = { term, fitAddon, wrapper, cleanup, webglLoaded: false };
  cache.set(ptyId, entry);
  return entry;
}

/** 按当前配置加载 LigaturesAddon。
 * 必须在 WebglAddon 之前调用 —— 否则 font-feature-settings 不会进 WebGL 纹理 atlas(上游 #5455)。
 * Windows/WebView2 用 queryLocalFonts 真实解析字体 calt 表;mac/Linux 无此 API 自动 fallback 到内置 Iosevka 60 条。
 */
function loadLigaturesIfEnabled(entry: CachedEntry): void {
  if (entry.ligaturesAddon) return;
  if (!useAppStore.getState().config.terminalLigatures) return;
  try {
    const lig = new LigaturesAddon();
    entry.term.loadAddon(lig);
    entry.ligaturesAddon = lig;
  } catch (e) {
    console.error('LigaturesAddon load failed', e);
  }
}

function disposeLigatures(entry: CachedEntry): void {
  if (entry.ligaturesAddon) {
    try { entry.ligaturesAddon.dispose(); } catch { /* 已 dispose */ }
    entry.ligaturesAddon = undefined;
  }
}

function disposeWebgl(entry: CachedEntry): void {
  if (entry.webglAddon) {
    try { entry.webglAddon.dispose(); } catch { /* 已 dispose */ }
    entry.webglAddon = undefined;
  }
  entry.webglLoaded = false;
}

/** 诊断开关:localStorage.miniterm.atlasDebug === '1' 时打印 atlas 事件日志。 */
function isAtlasDebugEnabled(): boolean {
  try {
    return typeof localStorage !== 'undefined' && localStorage.getItem('miniterm.atlasDebug') === '1';
  } catch {
    return false;
  }
}

function atlasDebugLog(tag: string, payload: Record<string, unknown>): void {
  if (!isAtlasDebugEnabled()) return;
  // eslint-disable-next-line no-console
  console.log(`[atlasDebug] ${tag}`, payload);
}

/** 读取 RenderService._isPaused(仅调试用,经 any 反射,xterm.js 私有字段)。 */
function readIsPaused(term: Terminal): boolean | null {
  try {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const rs = (term as any)._core?._renderService;
    return rs?._isPaused ?? null;
  } catch {
    return null;
  }
}

/**
 * atlas page 变更后,把 cache 中所有终端的可视行打 dirty,
 * 唤醒 dormant render loop 让其消费 atlas 的 _requestClearModel 兜底。
 *
 * 不可见终端(RenderService._isPaused === true)的 refresh 会被 xterm.js core 吞掉,
 * 仅设 _needsFullRefresh,需要靠 TerminalInstance 的 visibilityObserver 在可见性恢复时
 * 调 resetRenderStateForPty 兜底。
 */
function refreshAllTerminalsForAtlasChange(reason: 'add' | 'remove'): void {
  if (isAtlasDebugEnabled()) {
    atlasDebugLog('atlas-event', {
      reason,
      cacheSize: cache.size,
      terminals: Array.from(cache.entries()).map(([ptyId, e]) => ({
        ptyId,
        rows: e.term.rows,
        isPaused: readIsPaused(e.term),
      })),
    });
  }
  for (const e of cache.values()) {
    if (e.term.rows > 0) e.term.refresh(0, e.term.rows - 1);
  }
}

/**
 * 可见性恢复时(mount / IntersectionObserver intersecting)重建本终端的渲染状态:
 * 反射调 WebglRenderer 私有 _clearModel(true) 清空 RenderModel + GlyphRenderer 顶点缓冲,
 * 再 refresh 整屏 —— model 全空后每个 cell 的缓存比对必然失败,_updateModel(0, rows-1)
 * 全量 updateCell 按当前 atlas 重新取字形坐标,顶点数据与 atlas 重新对齐。
 *
 * 此前(v0.4.20~v0.8.2)这里调的是公开 API clearTextureAtlas(),它有致命副作用:
 * CharAtlasCache 按 (字体/字号/主题/DPR) 全局共享 TextureAtlas,clearTextureAtlas 会把
 * 共享 atlas 一并清空,却只修复本终端的 model。而底层 clearTexture() 既不发
 * add/remove 事件(v0.4.18 的广播 refresh 不触发)也不置 _requestClearModel(渲染时
 * beginFrame() 不触发全量重建),其余共享终端的顶点数据仍指向被清掉的旧坐标、
 * model 缓存又让 partial _updateModel 跳过所有未变 cell —— 静置终端在 atlas 按新
 * 布局重新填充后的下一次渲染整屏"换字"乱码,滚动(视口内容全变触发全量 diff)或
 * 重挂载才恢复。每修一个终端就投毒其余终端,这正是长时间静置分屏乱码的根因。
 *
 * _clearModel 路径不碰共享 atlas,共享 atlas 从此只增不清:页合并路径由上游粘性
 * _requestClearModel + add/remove 事件兜底,乱码链条从源头断开。
 */
export function resetRenderStateForPty(ptyId: number): void {
  const entry = cache.get(ptyId);
  if (!entry?.webglAddon) return;
  try {
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    const renderer = (entry.webglAddon as any)._renderer;
    if (typeof renderer?._clearModel === 'function') {
      renderer._clearModel(true);
      entry.term.refresh(0, entry.term.rows - 1);
      atlasDebugLog('clear-model', { ptyId, rows: entry.term.rows });
    } else {
      // 反射失效(addon 升级后私有字段改名)时退回公开 API;
      // 代价是共享 atlas 被清、其余终端可能延迟乱码,仅作兜底
      entry.webglAddon.clearTextureAtlas();
      atlasDebugLog('clear-atlas-fallback', { ptyId, rows: entry.term.rows });
    }
  } catch (e) {
    // eslint-disable-next-line no-console
    console.error('[atlasDebug] reset render state failed', e);
  }
}

function loadWebgl(entry: CachedEntry): void {
  if (entry.webglLoaded) return;
  entry.webglLoaded = true;
  try {
    const webgl = new WebglAddon();
    webgl.onContextLoss(() => {
      // 必须走 disposeWebgl 重置 webglLoaded,否则 context 丢失后 activateWebgl
      // 因 webglLoaded 仍为 true 永远空转,该终端只能退回 DOM 渲染直到进程重启
      disposeWebgl(entry);
      entry.term.refresh(0, entry.term.rows - 1);
    });
    webgl.onAddTextureAtlasCanvas(() => refreshAllTerminalsForAtlasChange('add'));
    webgl.onRemoveTextureAtlasCanvas(() => refreshAllTerminalsForAtlasChange('remove'));
    entry.term.loadAddon(webgl);
    entry.webglAddon = webgl;
  } catch {
    entry.webglLoaded = false;
  }
}

/** 在终端已挂载 DOM 并 fit 后激活 WebGL 渲染，降级时回退 Canvas。
 * ligatures 配置开启时,先加载 LigaturesAddon 再加 WebGL,保证 atlas 包含 calt glyph。
 */
export function activateWebgl(ptyId: number): void {
  const entry = cache.get(ptyId);
  if (!entry || entry.webglLoaded) return;
  loadLigaturesIfEnabled(entry);
  loadWebgl(entry);
}

/** 切换「启用连体字」开关或字体后,对单个已开终端重做 ligatures + WebGL 顺序。
 * 操作单帧内同步完成: dispose webgl → dispose ligatures → 按需 reload ligatures → reload webgl,
 * 全程无 await,避开 pty-output 全局监听器在 term.dispose 后写入的 race。
 * mount 流程尚未走完(webglLoaded=false)时跳过 —— activateWebgl 内会自然读到当前配置。
 */
export function reloadLigaturesForPty(ptyId: number): void {
  const entry = cache.get(ptyId);
  if (!entry || !entry.webglLoaded) return;
  disposeWebgl(entry);
  disposeLigatures(entry);
  loadLigaturesIfEnabled(entry);
  loadWebgl(entry);
  entry.term.refresh(0, entry.term.rows - 1);
}

/** 获取已缓存的终端（不创建新的） */
export function getCachedTerminal(ptyId: number): CachedTerminal | undefined {
  return cache.get(ptyId);
}

/** 终端内查找 addon 的存取（由 terminalSearch.ts 懒加载后寄存在此）。 */
export function getSearchAddon(ptyId: number): SearchAddon | undefined {
  return cache.get(ptyId)?.searchAddon;
}

export function setSearchAddon(ptyId: number, addon: SearchAddon): void {
  const entry = cache.get(ptyId);
  if (entry) entry.searchAddon = addon;
}

/** 彻底销毁终端（面板关闭 / kill_pty 后调用） */
export function disposeTerminal(ptyId: number): void {
  const entry = cache.get(ptyId);
  if (!entry) return;
  entry.wrapper.remove();
  entry.cleanup();
  cache.delete(ptyId);
  aiPtyIds.delete(ptyId);
  clearMarkerInstances(ptyId);
}

export function registerAiMarker(ptyId: number): IMarker | null {
  const cached = getCachedTerminal(ptyId);
  if (!cached) return null;
  // 备用缓冲区里打点没有意义,直接跳过:alt buffer 没有 scrollback 可滚,
  // 且 BufferSet.activateNormalBuffer() 退出时会 clearAllMarkers() 全部清掉。
  // 放行 alt screen(见 getOrCreateTerminal)后,走 TUI 的 AI 基本都落在这个分支。
  if (cached.term.buffer.active.type === 'alternate') return null;
  // -1:Enter 回显后光标已换行到下一行,取上一行即用户输入行本身
  const marker = cached.term.registerMarker(-1);
  if (!marker) return null;
  let inner = markerInstancesByPty.get(ptyId);
  if (!inner) {
    inner = new Map();
    markerInstancesByPty.set(ptyId, inner);
  }
  inner.set(marker.id, marker);
  marker.onDispose(() => {
    markerInstancesByPty.get(ptyId)?.delete(marker.id);
  });
  return marker;
}

export function scrollToMarker(ptyId: number, xtermMarkerId: number): void {
  const cached = getCachedTerminal(ptyId);
  const marker = markerInstancesByPty.get(ptyId)?.get(xtermMarkerId);
  if (!cached || !marker || marker.isDisposed) return;
  cached.term.scrollToLine(marker.line);
  flashLine(cached.term, marker);
}

function flashLine(term: Terminal, marker: IMarker): void {
  const deco: IDecoration | undefined = term.registerDecoration({
    marker,
    backgroundColor: FLASH_DECORATION_CSS_BG,
  });
  if (!deco) return;
  setTimeout(() => deco.dispose(), FLASH_DURATION_MS);
}

export function isMarkerDisposed(ptyId: number, xtermMarkerId: number): boolean {
  const marker = markerInstancesByPty.get(ptyId)?.get(xtermMarkerId);
  return !marker || marker.isDisposed;
}

export function clearMarkerInstances(ptyId: number): void {
  markerInstancesByPty.delete(ptyId);
}

export function updateAllTerminalThemes(terminalFollowTheme: boolean): void {
  const theme = getTerminalTheme(terminalFollowTheme);
  for (const entry of cache.values()) {
    entry.term.options.theme = theme;
  }
}

export function writePtyInput(ptyId: number, data: string): Promise<void> {
  return enqueuePtyWrite(ptyId, data);
}

/** 复制当前终端选中文本到系统剪贴板。无选中则不操作。返回是否有内容被复制。 */
export async function copyTerminalSelection(ptyId: number): Promise<boolean> {
  const cached = cache.get(ptyId);
  if (!cached) return false;
  const sel = cached.term.getSelection();
  if (!sel) return false;
  // 优先走 Webview 原生 Clipboard API(直接由 WebView 写系统剪贴板,
  // 不经过 Tauri IPC 的 JSON 序列化),避免长文本经 IPC 被截断。
  // 不可用时回退到 Tauri 插件 writeText。
  try {
    await navigator.clipboard.writeText(sel);
  } catch {
    await writeText(sel);
  }
  return true;
}

/** 检测剪贴板是否含图片（Tauri 插件 + 浏览器 Clipboard API 双重检测） */
async function clipboardHasImage(): Promise<boolean> {
  try {
    await readImage();
    return true;
  } catch { /* Tauri 插件不支持该格式 */ }
  try {
    const items = await navigator.clipboard.read();
    return items.some(item => item.types.some(t => t.startsWith('image/')));
  } catch { /* 浏览器 Clipboard API 不可用 */ }
  return false;
}

/** 判定剪贴板文本是否需要转存为临时文件（避免直接粘贴超长内容） */
function isLongText(text: string, lineThreshold: number, charThreshold: number): boolean {
  if (charThreshold > 0 && text.length >= charThreshold) return true;
  if (lineThreshold > 0) {
    const lines = text.replace(/\r\n/g, '\n').split('\n').length;
    if (lines >= lineThreshold) return true;
  }
  return false;
}

/** 正在处理粘贴的 pty。远程上传要几百毫秒到几秒，这期间用户连按 Ctrl+V 会让
 *  多条路径以完成顺序乱序插入命令行；直接丢弃重入的那次，语义上等同「还没粘完」。*/
const pasteInFlight = new Set<number>();

/** 上传/转换失败时弹一条提示（远程 pane 静默失败最难排查：粘进去的路径看着
 *  没毛病，只有远端 agent 报「文件不存在」）。仅在有项目归属时能弹。 */
function notifyPasteFailure(target: PasteTarget, err: unknown): void {
  console.error('粘贴内容转存到远端失败:', err);
  if (target.kind !== 'ssh') return;
  const detail = err instanceof Error ? err.message : String(err);
  useAppStore.getState().pushNotification({
    projectId: target.projectId,
    projectName: target.projectName,
    kind: 'paste-error',
    message: t('terminal.pasteUploadFailed', { detail }),
  });
}

/** 读取系统剪贴板并写入终端 PTY。
 * - 剪贴板含图片 → 保存为 temp PNG，粘贴带引号的路径（兼容含空格路径）
 * - 文本超过配置阈值且开关开启 → 保存为 temp .txt，粘贴带引号的路径
 * - 否则直接粘贴文本
 *
 * 落盘路径都要先过 {@link mapPastedFilePath}：WSL pane 转 `/mnt/...`，
 * SSH 远程 pane 走 SFTP 上传后粘远端路径（issue #36）。
 */
export async function pasteToTerminal(ptyId: number): Promise<void> {
  if (pasteInFlight.has(ptyId)) return;
  pasteInFlight.add(ptyId);
  try {
    await pasteToTerminalInner(ptyId);
  } finally {
    pasteInFlight.delete(ptyId);
  }
}

async function pasteToTerminalInner(ptyId: number): Promise<void> {
  const target = resolvePasteTarget(ptyId);

  if (await clipboardHasImage()) {
    // 优先：Win32 API 读取图片保存为 temp PNG，粘贴文件路径
    // 兼容 PinPix 等 arboard 无法读取的非标准剪贴板格式
    let localPath: string | null = null;
    try {
      localPath = await invoke<string>('read_clipboard_image');
    } catch { /* Win32 也读不到，回退 Alt+V */ }

    if (localPath !== null) {
      try {
        const path = await mapPastedFilePath(localPath, target);
        await enqueuePtyWrite(ptyId, `"${path}"`);
        return;
      } catch (e) {
        // 上传失败：明确告知。此时回退 Alt+V 也没用（远端 agent 读的是远端
        // 剪贴板），所以只提示、不往终端写任何东西。
        notifyPasteFailure(target, e);
        return;
      }
    }

    // 回退：发送 Alt+V 转义序列让 AI 工具自行处理
    await enqueuePtyWrite(ptyId, '\x1bv');
    return;
  }
  const text = await readText().catch(() => null);
  if (!text) return;

  const cfg = useAppStore.getState().config;
  const enabled = cfg.longPasteToFile ?? true;
  const lineThreshold = cfg.longPasteLineThreshold ?? 10;
  const charThreshold = cfg.longPasteCharThreshold ?? 2000;

  // 长文本：转存临时文件，粘贴路径；失败则回退到直接粘贴
  if (enabled && isLongText(text, lineThreshold, charThreshold)) {
    try {
      const localPath = await invoke<string>('save_clipboard_text', { text });
      const path = await mapPastedFilePath(localPath, target);
      await enqueuePtyWrite(ptyId, `"${path}"`);
      return;
    } catch (e) {
      // 本地写文件失败 → 直接粘原文（老行为）。
      // 远端上传失败 → 直接粘原文同样可用（就是长了点），比报错更有用；
      // 但仍要提示，否则用户不知道自己粘的是全文而非路径。
      notifyPasteFailure(target, e);
    }
  }

  const cached = getCachedTerminal(ptyId);
  if (cached) {
    cached.term.paste(text);
    return;
  }

  await enqueuePtyWrite(ptyId, text);
}
