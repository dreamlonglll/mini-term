import type { ModelPriceEntry } from '../types';

/**
 * 模型定价：浏览器 fetch models.dev（与 updateChecker.ts 同模式，免去 Rust 侧
 * 新增 HTTP 依赖），归一成 { model: {input, output, cacheRead, cacheWrite} }
 * （$/token，÷1e6）后经 invoke 传给后端查价。
 *
 * localStorage 缓存 24h TTL；拉取失败时用过期缓存兜底。失败且无缓存 → 抛错，
 * 由 UI 渲染错误占位 + Retry —— 绝不显示全 0 成本假数据。
 */

const PRICING_URL = 'https://models.dev/api.json';
const CACHE_KEY = 'mini-term-model-pricing';
const CACHE_TTL_MS = 24 * 60 * 60 * 1000;

/** 权威 provider：同名模型不被聚合商(openrouter 等)的记录覆盖 */
const CANONICAL_PROVIDERS = new Set(['anthropic', 'openai']);

interface CachedPricing {
  fetchedAt: number;
  table: Record<string, ModelPriceEntry>;
}

function normalize(api: unknown): Record<string, ModelPriceEntry> {
  const table: Record<string, ModelPriceEntry> = {};
  const fromCanonical = new Set<string>();
  if (typeof api !== 'object' || api === null) return table;

  for (const [providerId, provider] of Object.entries(api as Record<string, unknown>)) {
    const models = (provider as { models?: Record<string, unknown> })?.models;
    if (typeof models !== 'object' || models === null) continue;
    const canonical = CANONICAL_PROVIDERS.has(providerId);
    for (const [modelId, model] of Object.entries(models)) {
      const cost = (model as { cost?: Record<string, number> })?.cost;
      if (typeof cost?.input !== 'number' || typeof cost?.output !== 'number') continue;
      if (table[modelId] && (fromCanonical.has(modelId) || !canonical)) continue;
      table[modelId] = {
        input: cost.input / 1e6,
        output: cost.output / 1e6,
        cacheRead: (cost.cache_read ?? 0) / 1e6,
        cacheWrite: (cost.cache_write ?? 0) / 1e6,
      };
      if (canonical) fromCanonical.add(modelId);
    }
  }
  return table;
}

function readCache(): CachedPricing | null {
  try {
    const raw = localStorage.getItem(CACHE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as CachedPricing;
    if (typeof parsed?.fetchedAt !== 'number' || typeof parsed?.table !== 'object') return null;
    return parsed;
  } catch {
    return null;
  }
}

export async function loadModelPricing(): Promise<Record<string, ModelPriceEntry>> {
  const cached = readCache();
  if (cached && Date.now() - cached.fetchedAt < CACHE_TTL_MS) {
    return cached.table;
  }

  try {
    const resp = await fetch(PRICING_URL);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const table = normalize(await resp.json());
    if (Object.keys(table).length === 0) throw new Error('empty pricing table');
    try {
      localStorage.setItem(CACHE_KEY, JSON.stringify({ fetchedAt: Date.now(), table }));
    } catch {
      /* 缓存写失败不影响本次使用 */
    }
    return table;
  } catch (e) {
    // 过期缓存兜底：旧价也远好于无价
    if (cached) return cached.table;
    throw e;
  }
}
