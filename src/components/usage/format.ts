/** 金额：设计合同统一两位小数（千分位分组）；
 * 微额单独显示 <$0.01（不四舍五入成 $0.00 假象）。 */
export function formatCost(v: number): string {
  if (v >= 0.01) {
    return `$${v.toLocaleString('en-US', { minimumFractionDigits: 2, maximumFractionDigits: 2 })}`;
  }
  if (v > 0) return '<$0.01';
  return '$0';
}

/** token 数：K/M/B 三档缩写（设计合同） */
export function formatTokens(v: number): string {
  if (v >= 1e9) {
    const b = v / 1e9;
    return b >= 10 ? `${Math.round(b)}B` : `${b.toFixed(1)}B`;
  }
  if (v >= 1e6) {
    const m = v / 1e6;
    return m >= 10 ? `${Math.round(m)}M` : `${m.toFixed(1)}M`;
  }
  if (v >= 1e3) {
    const k = v / 1e3;
    return k >= 10 ? `${Math.round(k)}K` : `${k.toFixed(1)}K`;
  }
  return String(v);
}

/** 计数：千分位（44,011） */
export function formatCount(v: number): string {
  return v.toLocaleString('en-US');
}

/** 缓存命中率 = cacheRead ÷ (input + cacheRead + cacheWrite)，分母 0 → null */
export function cacheHitRate(input: number, cacheRead: number, cacheWrite: number): number | null {
  const denom = input + cacheRead + cacheWrite;
  if (denom <= 0) return null;
  return (cacheRead / denom) * 100;
}

/** 模型展示短名：通用规则推导，不维护映射表（新模型零改动）。
 * claude-opus-4-8 → "Opus 4.8"；claude-3-7-sonnet → "Sonnet 3.7"；
 * gpt-5-3-codex → "GPT-5.3 Codex"；未识别 → 原名 */
export function modelShortName(model: string): string {
  const cap = (s: string) => (s ? s[0].toUpperCase() + s.slice(1) : s);
  const parts = model.split('-');
  const isNum = (s: string) => /^\d+$/.test(s);
  if (parts[0] === 'claude' && parts.length >= 2) {
    const nums = parts.slice(1).filter(isNum);
    const words = parts.slice(1).filter((p) => !isNum(p)).map(cap);
    return [words.join(' '), nums.join('.')].filter(Boolean).join(' ') || model;
  }
  if (parts[0] === 'gpt' && parts.length >= 2) {
    const nums: string[] = [];
    const words: string[] = [];
    for (const p of parts.slice(1)) {
      if (isNum(p) && words.length === 0) nums.push(p);
      else words.push(cap(p));
    }
    return `GPT-${nums.join('.')}${words.length ? ' ' + words.join(' ') : ''}`;
  }
  return model;
}
