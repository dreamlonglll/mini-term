import { create } from 'zustand';
import { useMemo } from 'react';
import { dicts } from './locales';

export type Lang = 'zh' | 'en';

const STORAGE_KEY = 'mini-term-lang';

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
    /* SSR / 测试环境无 document */
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

// 模块加载时同步一次 <html lang>，与初始语言保持一致
applyHtmlLang(useI18nStore.getState().lang);

type Params = Record<string, string | number>;

/** 按点分路径在字典对象中查找字符串叶子节点 */
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

/** 把 {name} 占位符替换为 params 中的值 */
function interpolate(s: string, params?: Params): string {
  if (!params) return s;
  return s.replace(/\{(\w+)\}/g, (m, k) => (k in params ? String(params[k]) : m));
}

/** 翻译查找：当前语言 → 中文兜底 → 原 key 兜底 */
export function translate(lang: Lang, key: string, params?: Params): string {
  const raw = lookup(dicts[lang], key) ?? lookup(dicts.zh, key) ?? key;
  return interpolate(raw, params);
}

/**
 * 非组件上下文（工具函数、事件回调、toast、dialog）使用：
 * 直接读取当前语言。注意它不会订阅语言变化，仅适合调用时一次性求值的场景。
 */
export function t(key: string, params?: Params): string {
  return translate(useI18nStore.getState().lang, key, params);
}

/** React 组件使用：返回绑定当前语言的 t，语言切换时组件自动重渲染。 */
export function useT() {
  const lang = useI18nStore((s) => s.lang);
  return useMemo(
    () => (key: string, params?: Params) => translate(lang, key, params),
    [lang],
  );
}
