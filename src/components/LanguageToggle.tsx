import { useI18nStore, type Lang } from '../i18n';

/**
 * 中英语言切换段控件。
 * 选项文字用各自母语书写（中文 / English），属于不需要翻译的固定 endonym。
 */
export function LanguageToggle() {
  const lang = useI18nStore((s) => s.lang);
  const setLang = useI18nStore((s) => s.setLang);

  const options: { value: Lang; label: string }[] = [
    { value: 'zh', label: '中文' },
    { value: 'en', label: 'English' },
  ];

  return (
    <div className="inline-flex rounded-md overflow-hidden border border-[var(--border-default)]">
      {options.map((o) => (
        <button
          key={o.value}
          type="button"
          onClick={() => setLang(o.value)}
          className={`px-3 py-1 text-xs transition-colors duration-150 ${
            lang === o.value
              ? 'bg-[var(--accent)] text-white'
              : 'bg-transparent text-[var(--text-muted)] hover:text-[var(--text-primary)]'
          }`}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}
