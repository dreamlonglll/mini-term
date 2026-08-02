/**
 * 外置主题包（Dream Skin 兼容格式）的校验、token 映射与运行时应用。
 *
 * 应用机制照抄 fontManager 先例：documentElement.style.setProperty 批量覆盖
 * CSS 变量，清除时全量 removeProperty 回落 styles.css 静态基线，零残留。
 * `appearance` 决定 data-theme 取 dark/light（未覆盖的 token 回落到正确明暗基线），
 * data-skin 置空（由 App.tsx 的 skin effect 按 customThemeId 收敛）。
 */

import { invoke } from '@tauri-apps/api/core';
import { applyTheme } from './themeManager';
import { BUILTIN_TERMINAL_THEMES, type TerminalTheme } from './builtinThemes';

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
  image?: string;
  appearance: 'dark' | 'light';
  colors: ThemePackColors;
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

// ─── theme.json → mini-term token 映射（计划 3.2 映射表） ───

function buildTokenMap(def: ThemePackJson): Record<string, string> {
  const c = def.colors;
  const map: Record<string, string> = {
    '--bg-base': c.background,
    '--bg-terminal': c.background,
    '--bg-surface': c.panel,
    '--bg-elevated': c.panelAlt,
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
  };
  // 近似归宿：mt 暂无 accent-alt / secondary / highlight 独立 token（计划 3.2）
  if (c.accentAlt) map['--color-warning'] = c.accentAlt;
  if (c.secondary) map['--color-info'] = c.secondary;
  if (c.highlight) map['--color-success'] = c.highlight;
  // 逃生舱：tokens 直覆任意变量，优先级最高
  if (def.tokens) Object.assign(map, def.tokens);
  return map;
}

/** 缺省推导终端配色；ANSI 16 色取 appearance 对应内置基线（乱推会毁掉 TUI 可读性） */
function deriveTerminalTheme(def: ThemePackJson): TerminalTheme {
  const base = BUILTIN_TERMINAL_THEMES[def.appearance];
  const c = def.colors;
  return {
    ...base,
    background: c.background,
    foreground: c.text,
    cursor: c.accent,
    cursorAccent: c.background,
    selectionBackground: withAlpha(c.accent, 0.22) ?? base.selectionBackground,
    selectionForeground: c.text,
    ...def.terminal,
  };
}

// ─── 应用 / 清除 ───

let appliedProps: string[] = [];
let activeTheme: ThemePackJson | null = null;
let customTerminalTheme: TerminalTheme | null = null;

export function applyCustomTheme(def: ThemePackJson): void {
  clearCustomTheme();
  // 先落明暗基线：未覆盖的 token（diff/语法高亮等）回落到正确的 dark/light 静态规则
  applyTheme(def.appearance);
  const root = document.documentElement;
  root.dataset.customTheme = def.id;
  const map = buildTokenMap(def);
  for (const [prop, value] of Object.entries(map)) {
    root.style.setProperty(prop, value);
  }
  appliedProps = Object.keys(map);
  activeTheme = def;
  customTerminalTheme = deriveTerminalTheme(def);
}

/** 全量移除覆盖与标记。data-theme/data-skin 的回落由调用方走既有 theme/skin 链路。 */
export function clearCustomTheme(): void {
  const root = document.documentElement;
  for (const prop of appliedProps) {
    root.style.removeProperty(prop);
  }
  appliedProps = [];
  activeTheme = null;
  customTerminalTheme = null;
  delete root.dataset.customTheme;
}

/** 自定义主题激活时的终端配色；null = 未激活（getTerminalTheme 消费） */
export function getCustomTerminalTheme(): TerminalTheme | null {
  return customTerminalTheme;
}

export function getActiveCustomTheme(): ThemePackJson | null {
  return activeTheme;
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

/** 读取 + 校验 + 应用。任何一步失败都 throw，由调用方回落内置外观。 */
export async function loadAndApplyCustomTheme(themeId: string): Promise<ThemePackJson> {
  const data = await invoke<ThemePackData>('read_theme_pack', { themeId });
  const def = parseThemePack(themeId, data.themeJson);
  applyCustomTheme(def);
  return def;
}
