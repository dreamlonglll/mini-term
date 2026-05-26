import { invoke } from '@tauri-apps/api/core';

type ThemeMode = 'auto' | 'light' | 'dark';
type ResolvedTheme = 'light' | 'dark';

let currentResolved: ResolvedTheme = 'dark';
let cleanupFn: (() => void) | null = null;

function syncNativeTitleBar(theme: ResolvedTheme) {
  invoke('set_window_dark_mode', { dark: theme === 'dark' }).catch(() => {});
}

const STORAGE_KEY = 'mini-term-theme';
const COLOR_SCHEME_QUERY = '(prefers-color-scheme: light)';

type LegacyMediaQueryList = MediaQueryList & {
  addListener?: (listener: (event: MediaQueryListEvent) => void) => void;
  removeListener?: (listener: (event: MediaQueryListEvent) => void) => void;
};

function getColorSchemeQuery(): LegacyMediaQueryList | null {
  if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') {
    return null;
  }
  return window.matchMedia(COLOR_SCHEME_QUERY) as LegacyMediaQueryList;
}

function listenColorSchemeChange(
  mql: LegacyMediaQueryList,
  handler: (event: MediaQueryListEvent) => void,
): () => void {
  if (typeof mql.addEventListener === 'function') {
    mql.addEventListener('change', handler);
    return () => {
      mql.removeEventListener?.('change', handler);
    };
  }

  if (typeof mql.addListener === 'function') {
    mql.addListener(handler);
    return () => {
      mql.removeListener?.(handler);
    };
  }

  return () => {};
}

function resolveTheme(mode: ThemeMode): ResolvedTheme {
  if (mode === 'auto') {
    const mql = getColorSchemeQuery();
    return mql?.matches ? 'light' : 'dark';
  }
  return mode;
}

function applyToDOM(theme: ResolvedTheme) {
  currentResolved = theme;
  document.documentElement.dataset.theme = theme;
  localStorage.setItem(STORAGE_KEY, theme);
  syncNativeTitleBar(theme);
}

export function getResolvedTheme(): ResolvedTheme {
  return currentResolved;
}

export function applyTheme(mode: ThemeMode): void {
  if (cleanupFn) {
    cleanupFn();
    cleanupFn = null;
  }

  applyToDOM(resolveTheme(mode));

  if (mode === 'auto') {
    const mql = getColorSchemeQuery();
    if (!mql) return;
    const handler = (e: MediaQueryListEvent) => {
      applyToDOM(e.matches ? 'light' : 'dark');
      window.dispatchEvent(new CustomEvent('theme-changed', { detail: getResolvedTheme() }));
    };
    cleanupFn = listenColorSchemeChange(mql, handler);
  }
}
