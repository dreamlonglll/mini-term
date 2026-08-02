/**
 * 外置主题包（Dream Skin 兼容格式）的校验、token 映射与运行时应用。
 *
 * 应用机制照抄 fontManager 先例：documentElement.style.setProperty 批量覆盖
 * CSS 变量，清除时全量 removeProperty 回落 styles.css 静态基线，零残留。
 * `appearance` 决定 data-theme 取 dark/light（未覆盖的 token 回落到正确明暗基线），
 * data-skin 置空（由 App.tsx 的 skin effect 按 customThemeId 收敛）。
 *
 * Phase 2 背景图氛围层：背景图挂在 #root 的 inline background（html/body 的
 * 不透明 --bg-base 兜底在其后），表面透明组只把 surface/elevated/overlay/terminal
 * 四个 token 换成 rgba —— --bg-base 保持不透明，避免透出 WebView 底色。
 * Phase 3：theme.css 卫生检查后注入 <style data-mt-theme-css>（ds→mt 前缀转译）、
 * 主题目录 fs 监听热重载。
 */

import { convertFileSrc, invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { applyTheme } from './themeManager';
import { BUILTIN_TERMINAL_THEMES, type TerminalTheme } from './builtinThemes';
import type { FsChangePayload } from '../types';

/** theme.json 的 10 个语义色（Dream Skin 契约） */
export interface ThemePackColors {
  background: string;
  panel: string;
  panelAlt: string;
  accent: string;
  accentAlt?: string;
  secondary?: string;
  highlight?: string;
  text: string;
  muted: string;
  line: string;
}

export interface ThemePackJson {
  schemaVersion?: number;
  id: string;
  name: string;
  /** 背景图文件名（相对包目录）；无 = 纯 token 主题 */
  image?: string;
  appearance: 'dark' | 'light';
  /** 背景图构图：focusX/focusY ∈ [0,1]，图片焦点落在视口的位置 */
  art?: { focusX?: number; focusY?: number; safeArea?: string; taskMode?: string };
  colors: ThemePackColors;
  /** mini-term 扩展：氛围层旋钮（均可选） */
  effects?: {
    /** 面板表面不透明度，默认 0.85（仅带背景图时生效） */
    surfaceOpacity?: number;
    /** 背景图上的底色压暗层不透明度，默认 0.45 */
    backgroundDim?: number;
    /** 终端背景不透明度，默认取 surfaceOpacity */
    terminalOpacity?: number;
    /** theme.css 旋钮 --mt-theme-surface-radius / -blur 的取值 */
    surfaceRadius?: string;
    surfaceBlur?: string;
  };
  /** mini-term 扩展：完整/部分 xterm 24 字段，缺省走推导 */
  terminal?: Partial<TerminalTheme>;
  /** mini-term 扩展：直接覆盖任意 `--` 变量的逃生舱，优先级最高 */
  tokens?: Record<string, string>;
}

export interface ThemePackMeta {
  /** themes/ 下目录名（read_theme_pack 用它定位） */
  themeId: string;
  def: ThemePackJson;
}

// ─── 校验 ───

const REQUIRED_COLOR_KEYS = ['background', 'panel', 'panelAlt', 'accent', 'text', 'muted', 'line'] as const;
const OPTIONAL_COLOR_KEYS = ['accentAlt', 'secondary', 'highlight'] as const;

function isValidColor(value: unknown): value is string {
  return typeof value === 'string' && CSS.supports('color', value);
}

/** 解析并校验 theme.json 文本，不合法直接 throw（错误信息面向设置页展示） */
export function parseThemePack(themeId: string, jsonText: string): ThemePackJson {
  let raw: unknown;
  try {
    raw = JSON.parse(jsonText);
  } catch (e) {
    throw new Error(`theme.json 不是合法 JSON: ${e}`);
  }
  const def = raw as ThemePackJson;
  if (typeof def !== 'object' || def === null) throw new Error('theme.json 必须是对象');
  if (typeof def.id !== 'string' || !def.id) throw new Error('缺少 id 字段');
  if (typeof def.name !== 'string' || !def.name) throw new Error('缺少 name 字段');
  if (def.appearance !== 'dark' && def.appearance !== 'light') {
    throw new Error(`appearance 必须为 dark 或 light，实际: ${String(def.appearance)}`);
  }
  if (typeof def.colors !== 'object' || def.colors === null) throw new Error('缺少 colors 字段');
  for (const key of REQUIRED_COLOR_KEYS) {
    if (!isValidColor(def.colors[key])) {
      throw new Error(`colors.${key} 缺失或不是合法色值: ${String(def.colors[key])}`);
    }
  }
  for (const key of OPTIONAL_COLOR_KEYS) {
    if (def.colors[key] !== undefined && !isValidColor(def.colors[key])) {
      throw new Error(`colors.${key} 不是合法色值: ${String(def.colors[key])}`);
    }
  }
  if (def.tokens !== undefined && (typeof def.tokens !== 'object' || def.tokens === null)) {
    throw new Error('tokens 必须是对象');
  }
  if (def.image !== undefined && (typeof def.image !== 'string' || /[/\\]|\.\./.test(def.image))) {
    throw new Error(`image 必须是包内文件名: ${String(def.image)}`);
  }
  if (def.id !== themeId) {
    console.warn(`主题包目录名 ${themeId} 与 theme.json id ${def.id} 不一致，以目录名为准`);
  }
  return def;
}

// ─── 色彩派生 ───

interface Rgba { r: number; g: number; b: number; a: number }

/** 解析 #rgb/#rrggbb/#rrggbbaa 与 rgb()/rgba()；其余格式（命名色等）返回 null */
function parseColor(input: string): Rgba | null {
  const s = input.trim();
  const hex = /^#([0-9a-f]{3,8})$/i.exec(s)?.[1];
  if (hex) {
    if (hex.length === 3 || hex.length === 4) {
      const [r, g, b, a] = hex.split('').map((c) => parseInt(c + c, 16));
      return { r, g, b, a: hex.length === 4 ? a / 255 : 1 };
    }
    if (hex.length === 6 || hex.length === 8) {
      const n = (i: number) => parseInt(hex.slice(i, i + 2), 16);
      return { r: n(0), g: n(2), b: n(4), a: hex.length === 8 ? n(6) / 255 : 1 };
    }
    return null;
  }
  const m = /^rgba?\(\s*([\d.]+)\s*,\s*([\d.]+)\s*,\s*([\d.]+)\s*(?:,\s*([\d.]+)\s*)?\)$/i.exec(s);
  if (m) {
    return { r: +m[1], g: +m[2], b: +m[3], a: m[4] !== undefined ? +m[4] : 1 };
  }
  return null;
}

/** 把色值的透明度整体缩放到 factor 倍（clamp 到 1）。解析失败时原样返回。 */
function scaleAlpha(color: string, factor: number): string {
  const c = parseColor(color);
  if (!c) return color;
  const a = Math.min(1, c.a * factor);
  return `rgba(${c.r}, ${c.g}, ${c.b}, ${+a.toFixed(3)})`;
}

/** 以 alpha 生成派生色（xterm 也认这种 rgba 字符串）。解析失败返回 null。 */
function withAlpha(color: string, alpha: number): string | null {
  const c = parseColor(color);
  if (!c) return null;
  return `rgba(${c.r}, ${c.g}, ${c.b}, ${+(c.a * alpha).toFixed(3)})`;
}

// ─── 氛围层参数 ───

const DEFAULT_SURFACE_OPACITY = 0.85;
const DEFAULT_BACKGROUND_DIM = 0.45;

function hasBackgroundImage(def: ThemePackJson, dir: string | null): dir is string {
  return !!def.image && !!dir;
}

function surfaceOpacityOf(def: ThemePackJson): number {
  const v = def.effects?.surfaceOpacity;
  return typeof v === 'number' && v >= 0 && v <= 1 ? v : DEFAULT_SURFACE_OPACITY;
}

function terminalOpacityOf(def: ThemePackJson): number {
  const v = def.effects?.terminalOpacity;
  return typeof v === 'number' && v >= 0 && v <= 1 ? v : surfaceOpacityOf(def);
}

// ─── theme.json → mini-term token 映射（计划 3.2 映射表） ───

function buildTokenMap(def: ThemePackJson, withBackground: boolean): Record<string, string> {
  const c = def.colors;
  const so = surfaceOpacityOf(def);
  const map: Record<string, string> = {
    '--bg-base': c.background,
    '--bg-terminal': withBackground
      ? withAlpha(c.background, terminalOpacityOf(def)) ?? c.background
      : c.background,
    '--bg-surface': withBackground ? withAlpha(c.panel, so) ?? c.panel : c.panel,
    '--bg-elevated': withBackground ? withAlpha(c.panelAlt, so) ?? c.panelAlt : c.panelAlt,
    // 浮层始终保持不透明：弹窗/菜单叠在任意内容上，半透明会牺牲可读性
    '--bg-overlay': c.panelAlt,
    '--accent': c.accent,
    '--accent-muted': withAlpha(c.accent, 0.33) ?? c.accent,
    '--accent-subtle': withAlpha(c.accent, 0.18) ?? c.accent,
    '--text-primary': c.text,
    '--text-secondary': withAlpha(c.text, 0.75) ?? c.text,
    '--text-muted': c.muted,
    '--border-default': c.line,
    '--border-subtle': scaleAlpha(c.line, 0.6),
    '--border-strong': scaleAlpha(c.line, 1.4),
    // theme.css 旋钮变量（Phase 3，与 ds 的 --ds-theme-* 同构）
    '--mt-theme-color-background': c.background,
    '--mt-theme-color-panel': c.panel,
    '--mt-theme-color-panel-alt': c.panelAlt,
    '--mt-theme-color-accent': c.accent,
    '--mt-theme-color-text': c.text,
    '--mt-theme-color-muted': c.muted,
    '--mt-theme-color-line': c.line,
    '--mt-theme-surface-radius': def.effects?.surfaceRadius ?? '10px',
    '--mt-theme-surface-blur': def.effects?.surfaceBlur ?? '12px',
  };
  // 近似归宿：mt 暂无 accent-alt / secondary / highlight 独立 token（计划 3.2）
  if (c.accentAlt) {
    map['--color-warning'] = c.accentAlt;
    map['--mt-theme-color-accent-alt'] = c.accentAlt;
  }
  if (c.secondary) {
    map['--color-info'] = c.secondary;
    map['--mt-theme-color-secondary'] = c.secondary;
  }
  if (c.highlight) {
    map['--color-success'] = c.highlight;
    map['--mt-theme-color-highlight'] = c.highlight;
  }
  // 逃生舱：tokens 直覆任意变量，优先级最高
  if (def.tokens) Object.assign(map, def.tokens);
  return map;
}

/** 缺省推导终端配色；ANSI 16 色取 appearance 对应内置基线（乱推会毁掉 TUI 可读性） */
function deriveTerminalTheme(def: ThemePackJson, withBackground: boolean): TerminalTheme {
  const base = BUILTIN_TERMINAL_THEMES[def.appearance];
  const c = def.colors;
  return {
    ...base,
    background: withBackground
      ? withAlpha(c.background, terminalOpacityOf(def)) ?? c.background
      : c.background,
    foreground: c.text,
    cursor: c.accent,
    cursorAccent: c.background,
    selectionBackground: withAlpha(c.accent, 0.22) ?? base.selectionBackground,
    selectionForeground: c.text,
    ...def.terminal,
  };
}

// ─── theme.css 卫生检查与注入（Phase 3）───

const THEME_CSS_MAX_BYTES = 256 * 1024;
const STYLE_ATTR = 'data-mt-theme-css';

/** 本地信任模型下的轻量卫生检查：字节上限、禁 @import 与外链 url。
 *  同时把 Dream Skin 的 ds 前缀转译为 mt（选择器锚点与旋钮变量同构）。 */
function sanitizeThemeCss(css: string): string {
  if (new Blob([css]).size > THEME_CSS_MAX_BYTES) {
    throw new Error('theme.css 超过 256KB 上限');
  }
  if (/@import/i.test(css)) throw new Error('theme.css 不允许 @import');
  if (/url\(\s*['"]?\s*(?:https?:)?\/\//i.test(css)) {
    throw new Error('theme.css 不允许外链 url');
  }
  return css
    .split('data-ds-part').join('data-mt-part')
    .split('--ds-theme-').join('--mt-theme-');
}

function injectThemeCss(css: string | null): void {
  removeThemeCss();
  if (!css) return;
  const el = document.createElement('style');
  el.setAttribute(STYLE_ATTR, '');
  el.textContent = sanitizeThemeCss(css);
  document.head.appendChild(el);
}

function removeThemeCss(): void {
  document.head.querySelectorAll(`style[${STYLE_ATTR}]`).forEach((el) => el.remove());
}

// ─── 背景图氛围层（Phase 2）───

function applyBackgroundLayer(def: ThemePackJson, dir: string): void {
  const rootEl = document.getElementById('root');
  if (!rootEl) return;
  const url = convertFileSrc(`${dir}/${def.image}`);
  const dim = withAlpha(def.colors.background, def.effects?.backgroundDim ?? DEFAULT_BACKGROUND_DIM)
    ?? `rgba(0, 0, 0, ${DEFAULT_BACKGROUND_DIM})`;
  const focusX = def.art?.focusX ?? 0.5;
  const focusY = def.art?.focusY ?? 0.5;
  // 压暗层与图片合成在同一 background 上；background-color 仍由 styles.css 的
  // var(--bg-base) 兜底（图片加载完成前 / 加载失败时可见）
  rootEl.style.backgroundImage = `linear-gradient(${dim}, ${dim}), url("${url}")`;
  rootEl.style.backgroundSize = 'cover';
  rootEl.style.backgroundPosition = `${+(focusX * 100).toFixed(2)}% ${+(focusY * 100).toFixed(2)}%`;
  rootEl.style.backgroundRepeat = 'no-repeat';
  // 噪点层随背景主题归零（styles.css 的 :root[data-custom-theme-bg] 规则）
  document.documentElement.dataset.customThemeBg = '1';
}

function clearBackgroundLayer(): void {
  const rootEl = document.getElementById('root');
  if (rootEl) {
    rootEl.style.removeProperty('background-image');
    rootEl.style.removeProperty('background-size');
    rootEl.style.removeProperty('background-position');
    rootEl.style.removeProperty('background-repeat');
  }
  delete document.documentElement.dataset.customThemeBg;
}

// ─── 应用 / 清除 ───

/** 已应用变量清单同时记到 DOM，clear 不依赖模块内存（防 HMR 换模块后清不干净） */
const PROPS_ATTR = 'data-custom-theme-props';

let activeTheme: ThemePackJson | null = null;
let activeThemeDir: string | null = null;
let customTerminalTheme: TerminalTheme | null = null;

export function applyCustomTheme(def: ThemePackJson, dir: string | null = null, themeCss: string | null = null): void {
  clearCustomTheme();
  // 先落明暗基线：未覆盖的 token（diff/语法高亮等）回落到正确的 dark/light 静态规则
  applyTheme(def.appearance);
  const root = document.documentElement;
  root.dataset.customTheme = def.id;
  const withBg = hasBackgroundImage(def, dir);
  const map = buildTokenMap(def, withBg);
  for (const [prop, value] of Object.entries(map)) {
    root.style.setProperty(prop, value);
  }
  root.setAttribute(PROPS_ATTR, Object.keys(map).join(' '));
  if (withBg) applyBackgroundLayer(def, dir);
  // theme.css 不合法只警告不整包失败（token 主题仍可用）
  try {
    injectThemeCss(themeCss);
  } catch (e) {
    console.warn(`主题包 ${def.id} 的 theme.css 被拒绝:`, e);
  }
  activeTheme = def;
  activeThemeDir = dir;
  customTerminalTheme = deriveTerminalTheme(def, withBg);
}

/** 全量移除覆盖与标记。data-theme/data-skin 的回落由调用方走既有 theme/skin 链路。 */
export function clearCustomTheme(): void {
  const root = document.documentElement;
  const props = root.getAttribute(PROPS_ATTR)?.split(' ').filter(Boolean) ?? [];
  for (const prop of props) {
    root.style.removeProperty(prop);
  }
  root.removeAttribute(PROPS_ATTR);
  clearBackgroundLayer();
  removeThemeCss();
  activeTheme = null;
  activeThemeDir = null;
  customTerminalTheme = null;
  delete root.dataset.customTheme;
  activeThemeId = null;
  void unwatchThemeDir();
}

/** 自定义主题激活时的终端配色；null = 未激活（getTerminalTheme 消费） */
export function getCustomTerminalTheme(): TerminalTheme | null {
  return customTerminalTheme;
}

export function getActiveCustomTheme(): ThemePackJson | null {
  return activeTheme;
}

/** 带背景图的主题激活中 → 终端需要 allowTransparency（terminalCache 消费） */
export function isTransparentThemeActive(): boolean {
  return customTerminalTheme !== null && activeTheme?.image !== undefined && activeThemeDir !== null;
}

// ─── 与后端的读取链路 ───

interface ThemePackEntry { themeId: string; themeJson: string }
interface ThemePackData { themeJson: string; themeCss: string | null; dir: string }

/** 扫描 themes/ 目录，解析失败的包跳过并 console.warn（不阻塞列表） */
export async function listThemePacks(): Promise<ThemePackMeta[]> {
  const entries = await invoke<ThemePackEntry[]>('list_theme_packs');
  const out: ThemePackMeta[] = [];
  for (const entry of entries) {
    try {
      out.push({ themeId: entry.themeId, def: parseThemePack(entry.themeId, entry.themeJson) });
    } catch (e) {
      console.warn(`主题包 ${entry.themeId} 无效，已跳过:`, e);
    }
  }
  return out;
}

/** 读取 + 校验 + 应用 + 挂目录监听（热重载）。失败 throw，由调用方回落内置。 */
export async function loadAndApplyCustomTheme(themeId: string): Promise<ThemePackJson> {
  const data = await invoke<ThemePackData>('read_theme_pack', { themeId });
  const def = parseThemePack(themeId, data.themeJson);
  applyCustomTheme(def, data.dir, data.themeCss);
  activeThemeId = themeId;
  await watchThemeDir(data.dir);
  return def;
}

// ─── 主题目录热重载（Phase 3）───

/** watch_directory 复用项目文件监听通道，用哨兵 projectPath 区分主题事件 */
const THEME_WATCH_TAG = '__mt-theme-pack__';

let activeThemeId: string | null = null;
let watchedDir: string | null = null;
let watchListenerReady = false;
let reloadTimer: ReturnType<typeof setTimeout> | null = null;

async function watchThemeDir(dir: string): Promise<void> {
  ensureWatchListener();
  if (watchedDir === dir) return;
  await unwatchThemeDir();
  try {
    await invoke('watch_directory', { path: dir, projectPath: THEME_WATCH_TAG });
    watchedDir = dir;
  } catch (e) {
    console.warn('主题目录监听失败（热重载不可用）:', e);
  }
}

async function unwatchThemeDir(): Promise<void> {
  if (!watchedDir) return;
  const dir = watchedDir;
  watchedDir = null;
  try {
    await invoke('unwatch_directory', { path: dir });
  } catch { /* 目录已删或监听早已失效 */ }
}

function ensureWatchListener(): void {
  if (watchListenerReady) return;
  watchListenerReady = true;
  void listen<FsChangePayload>('fs-change', (event) => {
    if (event.payload.projectPath !== THEME_WATCH_TAG) return;
    // notify 会对一次保存吐多条事件，300ms 防抖后整包重载
    if (reloadTimer) clearTimeout(reloadTimer);
    reloadTimer = setTimeout(() => {
      reloadTimer = null;
      const id = activeThemeId;
      if (!id) return;
      loadAndApplyCustomTheme(id)
        .then(() => {
          // 终端配色的联动刷新由 App.tsx 监听此事件完成（避免 store 循环依赖）
          window.dispatchEvent(new CustomEvent('custom-theme-reloaded'));
        })
        .catch((e) => console.warn(`主题 ${id} 热重载失败（保留当前状态）:`, e));
    }, 300);
  });
}
