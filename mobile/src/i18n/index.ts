/**
 * PWA 轻量双语 i18n,与桌面端 src/i18n/store.ts 同模式:
 * zustand 存语言 + 点分路径字典查找 + {param} 插值,localStorage 持久化。
 */
import { create } from 'zustand';
import { useMemo } from 'react';
import { dicts } from './locales';

export type Lang = 'zh' | 'en';

const STORAGE_KEY = 'mt-mobile-lang';

function detectInitialLang(): Lang {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (saved === 'zh' || saved === 'en') return saved;
  } catch {
    /* localStorage 不可用时回退到自动探测 */
  }
  try {
    return navigator.language.toLowerCase().startsWith('zh') ? 'zh' : 'en';
  } catch {
    return 'zh';
  }
}

function applyHtmlLang(lang: Lang) {
  try {
    document.documentElement.lang = lang === 'zh' ? 'zh-CN' : 'en';
  } catch {
    /* 测试环境无 document */
  }
}

interface I18nStore {
  lang: Lang;
  setLang: (lang: Lang) => void;
}

export const useI18nStore = create<I18nStore>((set) => ({
  lang: detectInitialLang(),
  setLang: (lang) => {
    try {
      localStorage.setItem(STORAGE_KEY, lang);
    } catch {
      /* 持久化失败不影响运行时切换 */
    }
    applyHtmlLang(lang);
    set({ lang });
  },
}));

applyHtmlLang(useI18nStore.getState().lang);

type Params = Record<string, string | number>;

function lookup(dict: unknown, key: string): string | undefined {
  const parts = key.split('.');
  let cur: unknown = dict;
  for (const p of parts) {
    if (cur && typeof cur === 'object' && p in (cur as Record<string, unknown>)) {
      cur = (cur as Record<string, unknown>)[p];
    } else {
      return undefined;
    }
  }
  return typeof cur === 'string' ? cur : undefined;
}

function interpolate(s: string, params?: Params): string {
  if (!params) return s;
  return s.replace(/\{(\w+)\}/g, (m, k) => (k in params ? String(params[k]) : m));
}

export function translate(lang: Lang, key: string, params?: Params): string {
  const raw = lookup(dicts[lang], key) ?? lookup(dicts.zh, key) ?? key;
  return interpolate(raw, params);
}

/** 非组件上下文使用:一次性求值,不订阅语言变化。 */
export function t(key: string, params?: Params): string {
  return translate(useI18nStore.getState().lang, key, params);
}

/** React 组件使用:语言切换时自动重渲染。 */
export function useT() {
  const lang = useI18nStore((s) => s.lang);
  return useMemo(
    () => (key: string, params?: Params) => translate(lang, key, params),
    [lang],
  );
}
