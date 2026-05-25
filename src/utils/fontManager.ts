/**
 * 把 `uiFontFamily` 写入两个 CSS 变量，让普通文本和 `font-mono` / `.md-preview code`
 * 等等宽位置共用一个字体（"单字段覆盖整个 UI"决策，对应 issue 32 后续讨论）。
 *
 * 传入 undefined / 空串时移除 inline style，回退到 styles.css `:root` 默认值
 * （主文本 DM Sans，等宽 JetBrains Mono fallback 链）。
 */
export function applyUiFontFamily(value: string | undefined): void {
  const root = document.documentElement;
  const trimmed = value?.trim();
  if (trimmed) {
    root.style.setProperty('--app-font-family', trimmed);
    root.style.setProperty('--app-font-mono', trimmed);
  } else {
    root.style.removeProperty('--app-font-family');
    root.style.removeProperty('--app-font-mono');
  }
}
