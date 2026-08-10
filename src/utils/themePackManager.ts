/**
 * 外置主题包（Dream Skin 兼容格式）的校验、token 映射与运行时应用。
 *
 * 应用机制照抄 fontManager 先例：documentElement.style.setProperty 批量覆盖
 * CSS 变量，清除时全量 removeProperty 回落 styles.css 静态基线，零残留。
 *
 * 明暗语义：皮肤的明暗由作者在 theme.json 的 appearance 定死，激活时
 * data-theme 固定为该态（未覆盖的 token 回落对应明暗基线），主题按钮
 * 置为未选中态；切换主题 = 退出皮肤回内置。
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
    /** 终端区着色层不透明度，默认 0.6（着色只画一层在 --bg-terminal） */
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

/** buildTokenMap/deriveTerminalTheme 消费的字段组（即 theme.json 相应顶层字段） */
interface ThemeVariantDef {
  colors: ThemePackColors;
  effects?: ThemePackJson['effects'];
  terminal?: Partial<TerminalTheme>;
  tokens?: Record<string, string>;
}

export interface ThemePackMeta {
  /** themes/ 下目录名（read_theme_pack 用它定位） */
  themeId: string;
  def: ThemePackJson;
  /** 包目录绝对路径，卡片背景缩略图用 convertFileSrc 组 URL */
  dir: string;
}

// ─── 校验 ───

const REQUIRED_COLOR_KEYS = ['background', 'panel', 'panelAlt', 'accent', 'text', 'muted', 'line'] as const;
const OPTIONAL_COLOR_KEYS = ['accentAlt', 'secondary', 'highlight'] as const;

function isValidColor(value: unknown): value is string {
  return typeof value === 'string' && CSS.supports('color', value);
}

function validateColors(colors: unknown, label: string): asserts colors is ThemePackColors {
  if (typeof colors !== 'object' || colors === null) throw new Error(`缺少 ${label} 字段`);
  const c = colors as Record<string, unknown>;
  for (const key of REQUIRED_COLOR_KEYS) {
    if (!isValidColor(c[key])) {
      throw new Error(`${label}.${key} 缺失或不是合法色值: ${String(c[key])}`);
    }
  }
  for (const key of OPTIONAL_COLOR_KEYS) {
    if (c[key] !== undefined && !isValidColor(c[key])) {
      throw new Error(`${label}.${key} 不是合法色值: ${String(c[key])}`);
    }
  }
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
  validateColors(def.colors, 'colors');
  if (def.tokens !== undefined) {
    if (typeof def.tokens !== 'object' || def.tokens === null) {
      throw new Error('tokens 必须是对象');
    }
    validateTokenOverrides(def.tokens as Record<string, unknown>, 'tokens');
  }
  // terminal 直接喂给 xterm，坏色值会让 setTheme 抛在 updateAllTerminalThemes
  // 半途——换主题只改到一半，剩下的终端停在旧配色。与 colors 同一把尺子先拦掉。
  if (def.terminal !== undefined) {
    if (typeof def.terminal !== 'object' || def.terminal === null) {
      throw new Error('terminal 必须是对象');
    }
    for (const [key, value] of Object.entries(def.terminal)) {
      if (!isValidColor(value)) {
        throw new Error(`terminal.${key} 不是合法色值: ${String(value)}`);
      }
    }
  }
  // 这两个旋钮进的是 CSS 变量，theme.css 里一句 `background: var(--mt-theme-surface-blur)`
  // 就能把它当引用使——与 tokens 同源的敞口，同样走外链闸
  for (const key of ['surfaceRadius', 'surfaceBlur'] as const) {
    const v = def.effects?.[key];
    if (v === undefined) continue;
    if (typeof v !== 'string') throw new Error(`effects.${key} 必须是字符串`);
    assertNoRemoteRef(decodeCssEscapes(v), `effects.${key}`);
  }
  // 空串在这里归一化掉而不是放行：`image: ""` 曾能通过校验，此后
  // hasBackgroundImage（`!!def.image`）说"没有背景图"、isTransparentThemeActive
  // （`image !== undefined`）说"有"，终端被透明化并丢掉 WebGL，背景层却没挂。
  if (def.image !== undefined && typeof def.image === 'string' && !def.image.trim()) {
    delete def.image;
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

// 氛围可见度：压暗层与表面 alpha 是乘性叠加，默认值取「图可见 + 文字可读」平衡点
const DEFAULT_SURFACE_OPACITY = 0.72;
const DEFAULT_BACKGROUND_DIM = 0.35;
const DEFAULT_TERMINAL_OPACITY = 0.6;

function hasBackgroundImage(def: ThemePackJson, dir: string | null): dir is string {
  return !!def.image && !!dir;
}

type ThemeEffects = ThemePackJson['effects'];

function surfaceOpacityOf(effects: ThemeEffects): number {
  const v = effects?.surfaceOpacity;
  return typeof v === 'number' && v >= 0 && v <= 1 ? v : DEFAULT_SURFACE_OPACITY;
}

function terminalOpacityOf(effects: ThemeEffects): number {
  const v = effects?.terminalOpacity;
  return typeof v === 'number' && v >= 0 && v <= 1 ? v : DEFAULT_TERMINAL_OPACITY;
}

// ─── theme.json → mini-term token 映射（计划 3.2 映射表） ───

function buildTokenMap(variant: ThemeVariantDef, withBackground: boolean): Record<string, string> {
  const c = variant.colors;
  const so = surfaceOpacityOf(variant.effects);
  const map: Record<string, string> = {
    '--bg-base': c.background,
    '--bg-terminal': withBackground
      ? withAlpha(c.background, terminalOpacityOf(variant.effects)) ?? c.background
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
    // 背景图模式下旋钮面板色同样带 surfaceOpacity:theme.css 用它直接刷
    // 容器背景(如 sidebar),给不透明原色会与 --bg-surface 半透明面板割裂
    '--mt-theme-color-panel': withBackground ? withAlpha(c.panel, so) ?? c.panel : c.panel,
    '--mt-theme-color-panel-alt': withBackground ? withAlpha(c.panelAlt, so) ?? c.panelAlt : c.panelAlt,
    '--mt-theme-color-accent': c.accent,
    '--mt-theme-color-text': c.text,
    '--mt-theme-color-muted': c.muted,
    '--mt-theme-color-line': c.line,
    '--mt-theme-surface-radius': variant.effects?.surfaceRadius ?? '10px',
    '--mt-theme-surface-blur': variant.effects?.surfaceBlur ?? '12px',
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
  if (variant.tokens) Object.assign(map, variant.tokens);
  return map;
}

/** 缺省推导终端配色；ANSI 16 色取当前明暗态的内置基线（乱推会毁掉 TUI 可读性） */
function deriveTerminalTheme(variant: ThemeVariantDef, mode: 'dark' | 'light', withBackground: boolean): TerminalTheme {
  const base = BUILTIN_TERMINAL_THEMES[mode];
  const c = variant.colors;
  // 带背景图时丢掉作者写的 terminal.background:overrides 在展开顺序上排在下面
  // 那次透明化之后,一个照着内置主题抄全 24 字段的皮肤会把氛围图整块盖死,
  // 而且没有任何提示——声明里写的是"完整/部分 xterm 24 字段",抄全是自然做法
  const overrides = { ...variant.terminal };
  if (withBackground) delete overrides.background;
  return {
    ...base,
    // 背景图模式下 xterm 自身背景全透明（保留 RGB 供对比度计算），
    // 着色统一由 --bg-terminal 容器层承担，避免容器/wrapper/xterm 三层叠加
    background: withBackground ? withAlpha(c.background, 0) ?? c.background : c.background,
    foreground: c.text,
    cursor: c.accent,
    cursorAccent: c.background,
    selectionBackground: withAlpha(c.accent, 0.22) ?? base.selectionBackground,
    selectionForeground: c.text,
    ...overrides,
  };
}

// ─── theme.css 卫生检查与注入（Phase 3）───

const THEME_CSS_MAX_BYTES = 256 * 1024;
const STYLE_ATTR = 'data-mt-theme-css';

/** 还原 CSS 转义序列（`\68 ` → `h`、`\.` → `.`），**仅供卫生检查取样**。
 *
 *  注入的仍是原文：CSS 词法层在解析时本就会做这一步，于是 `@\69 mport` 与
 *  `url(\68 ttps://evil/x.png)` 对浏览器等价于 `@import` / `url(https://…)`，
 *  而正则直接打在原文上一个都拦不住（三个 payload 实测全部放行）。 */
function decodeCssEscapes(css: string): string {
  return css.replace(/\\(?:([0-9a-fA-F]{1,6})[ \t\n\r\f]?|([^]))/g, (_m, hex, ch) => {
    if (!hex) return ch;
    const cp = parseInt(hex, 16);
    // CSS 规定 0 / 代理区 / 超出 Unicode 范围一律替换为 U+FFFD
    return cp === 0 || cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)
      ? '�'
      : String.fromCodePoint(cp);
  });
}

/** 注释里的 URL 是说明不是引用，取样前先剥掉，免得 `/* 见 https://… *​/` 被误杀。 */
function stripCssComments(css: string): string {
  return css.replace(/\/\*[^]*?\*\//g, ' ');
}

/** 引用是否指向包外：带 scheme（data: 除外）或协议相对的 `//host/…`。 */
function isRemoteRef(ref: string): boolean {
  const s = ref.trim();
  if (!s || s.startsWith('data:')) return false;
  return s.startsWith('//') || /^[a-z][a-z0-9+.-]*:/i.test(s);
}

/** 外链闸：`url()` 与**裸字符串字面量**双查，供 theme.css 与 tokens 共用。
 *
 *  皮肤是从别处下载来的共享产物（自带 zip 导入），而 `tauri.conf.json` 的 csp
 *  是 null，没有第二道防线——一个外链就能把应用启动时机与 IP 回传。
 *
 *  只查 `url()` 封不住：Chromium 认 `image-set("https://…" 1x)` 这种不带 url()
 *  的裸字符串写法，`@font-face` 的 src 同理，一样发请求。所以字符串字面量一并查。
 *  代价是 `content: "https://…"` 这类纯文本展示也会被拒——本地皮肤里没有正当
 *  用途，宁可误杀。检查跑在**剥注释 + 转义还原后**的取样上，见 decodeCssEscapes。 */
function assertNoRemoteRef(probe: string, label: string): void {
  for (const m of probe.matchAll(/url\(\s*(['"]?)([^'")]*)\1\s*\)/gi)) {
    if (isRemoteRef(m[2])) {
      throw new Error(`${label} 的 url() 只允许包内相对路径或 data:，实际: ${m[2].trim()}`);
    }
  }
  for (const m of probe.matchAll(/(['"])((?:(?!\1).)*?)\1/g)) {
    if (isRemoteRef(m[2])) {
      throw new Error(`${label} 不允许指向包外的引用: ${m[2].trim()}`);
    }
  }
}

/** theme.css 的卫生检查：字节上限、禁 @import、禁外链。
 *
 *  同时把 Dream Skin 的 ds 前缀转译为 mt（选择器锚点与旋钮变量同构）。 */
export function sanitizeThemeCss(css: string): string {
  if (new Blob([css]).size > THEME_CSS_MAX_BYTES) {
    throw new Error('theme.css 超过 256KB 上限');
  }
  const probe = decodeCssEscapes(stripCssComments(css));
  if (/@import/i.test(probe)) throw new Error('theme.css 不允许 @import');
  assertNoRemoteRef(probe, 'theme.css');
  return css
    .split('data-ds-part').join('data-mt-part')
    .split('--ds-theme-').join('--mt-theme-');
}

/** tokens 逃生舱的闸：只许覆盖 `--` 自定义属性，值不许指向包外。
 *
 *  少了它，tokens 就是上面那道白名单的绕行道——键名不以 `--` 开头时
 *  `setProperty` 设的是**真实 CSS 属性**，`{"background-image":"url(https://…)"}`
 *  一行就够让 documentElement 去拉外部资源，而 theme.css 那边正为同一件事做
 *  转义还原加白名单。同一个包、同一个威胁，两处口径必须一致。
 *
 *  键名限死还顺带保住了 PROPS_ATTR——它用空格 join 键名，带空格的键会让
 *  clearAppliedDom 的 split(' ') 清不干净。 */
function validateTokenOverrides(tokens: Record<string, unknown>, label: string): void {
  for (const [key, value] of Object.entries(tokens)) {
    if (!/^--[A-Za-z0-9_-]+$/.test(key)) {
      throw new Error(`${label} 只能覆盖 -- 开头的 CSS 变量，非法键名: ${key}`);
    }
    if (typeof value !== 'string') {
      throw new Error(`${label}.${key} 必须是字符串`);
    }
    assertNoRemoteRef(decodeCssEscapes(value), `${label}.${key}`);
  }
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

/** 包内资源的可用 URL 缓存（asset 探活或 base64 兜底的结果，按 themeId/file 记） */
const assetUrlCache = new Map<string, string>();

/** 丢弃某个主题（或全部）的资源 URL 缓存。
 *  热重载与同名重导入都会换掉文件内容，缓存不清的话缩略图还是旧图 —— asset
 *  协议的 URL 逐字不变，base64 兜底更是把旧字节整个留在内存里。 */
export function invalidateThemeAssets(themeId?: string): void {
  if (themeId === undefined) {
    assetUrlCache.clear();
    return;
  }
  for (const key of [...assetUrlCache.keys()]) {
    if (key.startsWith(`${themeId}/`)) assetUrlCache.delete(key);
  }
}

/** 解析包内资源的可显示 URL：优先 asset 协议，加载失败回退 base64 数据 URL。
 *  设置页皮肤卡片缩略图使用；结果缓存避免重复 IPC 大文件。 */
export async function resolveThemeAssetUrl(dir: string, themeId: string, file: string): Promise<string> {
  const key = `${themeId}/${file}`;
  const cached = assetUrlCache.get(key);
  if (cached) return cached;
  const assetUrl = convertFileSrc(`${dir}/${file}`);
  const ok = await new Promise<boolean>((resolve) => {
    // 部分 WebView 环境对 asset 协议既不 onload 也不 onerror(静默挂起),
    // 无超时会卡住整个解析、base64 兜底永远走不到,卡片静默无图;
    // 超时一律视为「asset 不可用」落兜底(本地文件正常加载仅数十 ms)
    const probe = new Image();
    const timer = window.setTimeout(() => resolve(false), 1500);
    probe.onload = () => {
      window.clearTimeout(timer);
      resolve(true);
    };
    probe.onerror = () => {
      window.clearTimeout(timer);
      resolve(false);
    };
    probe.src = assetUrl;
  });
  const url = ok
    ? assetUrl
    : `data:${mimeOf(file)};base64,${await invoke<string>('read_theme_asset', { themeId, file })}`;
  assetUrlCache.set(key, url);
  return url;
}

/** 探活令牌：主题切换后作废在途的 base64 兜底回填 */
let bgProbeToken = 0;

function setRootBackground(rootEl: HTMLElement, def: ThemePackJson, variant: ThemeVariantDef, imageUrl: string): void {
  // 压暗层用当前变体底色：浅色态自动变成浅纱罩
  const dim = withAlpha(variant.colors.background, variant.effects?.backgroundDim ?? DEFAULT_BACKGROUND_DIM)
    ?? `rgba(0, 0, 0, ${DEFAULT_BACKGROUND_DIM})`;
  const focusX = def.art?.focusX ?? 0.5;
  const focusY = def.art?.focusY ?? 0.5;
  // 压暗层与图片合成在同一 background 上；background-color 仍由 styles.css 的
  // var(--bg-base) 兜底（图片加载完成前 / 加载失败时可见）
  rootEl.style.backgroundImage = `linear-gradient(${dim}, ${dim}), url("${imageUrl}")`;
  rootEl.style.backgroundSize = 'cover';
  rootEl.style.backgroundPosition = `${+(focusX * 100).toFixed(2)}% ${+(focusY * 100).toFixed(2)}%`;
  rootEl.style.backgroundRepeat = 'no-repeat';
}

function mimeOf(file: string): string {
  const f = file.toLowerCase();
  if (f.endsWith('.png')) return 'image/png';
  if (f.endsWith('.webp')) return 'image/webp';
  if (f.endsWith('.gif')) return 'image/gif';
  return 'image/jpeg';
}

function applyBackgroundLayer(def: ThemePackJson, variant: ThemeVariantDef, dir: string): void {
  const rootEl = document.getElementById('root');
  if (!rootEl) return;
  const image = def.image!;
  const themeId = dir.split(/[/\\]/).filter(Boolean).pop() ?? def.id;
  const cacheKey = `${themeId}/${image}`;
  // 缩略图（resolveThemeAssetUrl）已解析过同一文件时直接用结论,
  // 不再探活、也不再对同一文件各发一次 read_theme_asset IPC
  const cached = assetUrlCache.get(cacheKey);
  if (cached) {
    setRootBackground(rootEl, def, variant, cached);
    document.documentElement.dataset.customThemeBg = '1';
    return;
  }
  const url = convertFileSrc(`${dir}/${image}`);
  setRootBackground(rootEl, def, variant, url);
  // 噪点层随背景主题归零（styles.css 的 :root[data-custom-theme-bg] 规则）
  document.documentElement.dataset.customThemeBg = '1';

  // 探活：CSS 背景图加载失败是静默的，用 Image 预载检测 asset URL；
  // 失败则走 Rust 读文件转 base64 数据 URL 兜底，并把根因留在控制台。
  // 两个分支的结论都进 assetUrlCache 与缩略图共享（按文件 key,主题切换不影响其有效性）
  const token = ++bgProbeToken;
  const probe = new Image();
  probe.onload = () => {
    assetUrlCache.set(cacheKey, url);
  };
  probe.onerror = () => {
    if (token !== bgProbeToken) return;
    console.warn(`背景图 asset URL 加载失败，回退 base64 通道: ${url}`);
    invoke<string>('read_theme_asset', { themeId, file: image })
      .then((b64) => {
        const dataUrl = `data:${mimeOf(image)};base64,${b64}`;
        assetUrlCache.set(cacheKey, dataUrl);
        if (token !== bgProbeToken) return;
        setRootBackground(rootEl, def, variant, dataUrl);
      })
      .catch((e) => console.error('背景图兜底读取也失败:', e));
  };
  probe.src = url;
}

function clearBackgroundLayer(): void {
  bgProbeToken++;
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
  clearAppliedDom();
  // 皮肤明暗由 appearance 定死：先落对应明暗基线，未覆盖的 token
  // （diff/语法高亮等）回落正确的 dark/light 静态规则
  const mode = def.appearance;
  applyTheme(mode);
  const variant: ThemeVariantDef = { colors: def.colors, effects: def.effects, terminal: def.terminal, tokens: def.tokens };
  const root = document.documentElement;
  root.dataset.customTheme = def.id;
  const withBg = hasBackgroundImage(def, dir);
  const map = buildTokenMap(variant, withBg);
  for (const [prop, value] of Object.entries(map)) {
    root.style.setProperty(prop, value);
  }
  root.setAttribute(PROPS_ATTR, Object.keys(map).join(' '));
  if (withBg) applyBackgroundLayer(def, variant, dir);
  // theme.css 不合法只警告不整包失败（token 主题仍可用）
  try {
    injectThemeCss(themeCss);
  } catch (e) {
    console.warn(`主题包 ${def.id} 的 theme.css 被拒绝:`, e);
  }
  activeTheme = def;
  activeThemeDir = dir;
  customTerminalTheme = deriveTerminalTheme(variant, mode, withBg);
}

/** 只清 DOM 覆盖（变量/背景层/注入 CSS/标记），保留监听与激活态。
 *  供 applyCustomTheme 重复应用同一主题（热重载）时复用，监听不受影响。 */
function clearAppliedDom(): void {
  const root = document.documentElement;
  const props = root.getAttribute(PROPS_ATTR)?.split(' ').filter(Boolean) ?? [];
  for (const prop of props) {
    root.style.removeProperty(prop);
  }
  root.removeAttribute(PROPS_ATTR);
  clearBackgroundLayer();
  removeThemeCss();
  delete root.dataset.customTheme;
}

/** 完全停用：DOM 覆盖 + 激活态 + 目录监听全清。
 *  data-theme/data-skin 的回落由调用方走既有 theme/skin 链路。 */
export function clearCustomTheme(): void {
  clearAppliedDom();
  activeTheme = null;
  activeThemeDir = null;
  customTerminalTheme = null;
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

interface ThemePackEntry { themeId: string; themeJson: string; dir: string }
interface ThemePackData { themeJson: string; themeCss: string | null; dir: string }

/** 扫描 themes/ 目录，解析失败的包跳过并 console.warn（不阻塞列表） */
export async function listThemePacks(): Promise<ThemePackMeta[]> {
  const entries = await invoke<ThemePackEntry[]>('list_theme_packs');
  const out: ThemePackMeta[] = [];
  for (const entry of entries) {
    try {
      out.push({ themeId: entry.themeId, def: parseThemePack(entry.themeId, entry.themeJson), dir: entry.dir });
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

/** watch/unwatch 的串行闸。`watchedDir === dir` 的判断与赋值之间隔着两个 await，
 *  快速连点两个主题会让两次调用双双越过判断、各注册一个目录，而 watchedDir 只
 *  留得住后写入的那个 —— 另一个永不 unwatch。 */
let watchQueue: Promise<unknown> = Promise.resolve();

function serializeWatch<T>(task: () => Promise<T>): Promise<T> {
  const run = watchQueue.then(task, task);
  watchQueue = run.catch(() => {});
  return run;
}

function watchThemeDir(dir: string): Promise<void> {
  ensureWatchListener();
  return serializeWatch(async () => {
    if (watchedDir === dir) return;
    await unwatchInner();
    try {
      await invoke('watch_directory', { path: dir, projectPath: THEME_WATCH_TAG });
      watchedDir = dir;
    } catch (e) {
      console.warn('主题目录监听失败（热重载不可用）:', e);
    }
  });
}

function unwatchThemeDir(): Promise<void> {
  return serializeWatch(unwatchInner);
}

async function unwatchInner(): Promise<void> {
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
      // 重载前丢掉该包的资源缓存：改的很可能就是背景图，缓存留着会拿旧图重挂
      invalidateThemeAssets(id);
      loadAndApplyCustomTheme(id)
        .then(() => {
          // 终端配色的联动刷新由 App.tsx 监听此事件完成（避免 store 循环依赖）
          window.dispatchEvent(new CustomEvent('custom-theme-reloaded'));
        })
        .catch((e) => console.warn(`主题 ${id} 热重载失败（保留当前状态）:`, e));
    }, 300);
  });
}
