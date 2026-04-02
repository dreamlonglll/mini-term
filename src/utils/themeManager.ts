type ThemeMode = 'auto' | 'light' | 'dark';
type ResolvedTheme = 'light' | 'dark';

let currentResolved: ResolvedTheme = 'dark';
let cleanupFn: (() => void) | null = null;

const STORAGE_KEY = 'mini-term-theme';

function resolveTheme(mode: ThemeMode): ResolvedTheme {
  if (mode === 'auto') {
    return window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
  }
  return mode;
}

function applyToDOM(theme: ResolvedTheme) {
  currentResolved = theme;
  document.documentElement.dataset.theme = theme;
  localStorage.setItem(STORAGE_KEY, theme);
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
    const mql = window.matchMedia('(prefers-color-scheme: light)');
    const handler = (e: MediaQueryListEvent) => {
      applyToDOM(e.matches ? 'light' : 'dark');
      window.dispatchEvent(new CustomEvent('theme-changed', { detail: getResolvedTheme() }));
    };
    mql.addEventListener('change', handler);
    cleanupFn = () => mql.removeEventListener('change', handler);
  }
}
