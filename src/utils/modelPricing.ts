import type { ModelPriceEntry } from '../types';

/**
 * 模型定价：浏览器 fetch models.dev（与 updateChecker.ts 同模式，免去 Rust 侧
 * 新增 HTTP 依赖），归一成 { canonical 模型名: {input, output, cacheRead, cacheWrite} }
 * （$/token，÷1e6）后经 invoke 传给后端查价。
 *
 * localStorage 缓存 24h TTL；拉取失败时用过期缓存兜底。失败且无缓存 → 抛错，
 * 由 UI 渲染错误占位 + Retry —— 绝不显示全 0 成本假数据。
 *
 * **建键按 canonical 形式**（与后端 `pricing.rs` 的 `canonical()` 同规则）：
 * models.dev 是 provider → models 两层结构，同一个模型会被几十家 provider 以
 * 不同 id、不同价登记（`claude-opus-5` / `anthropic/claude-opus-5` /
 * `claude-opus-5@eu` …）。按原始 modelId 建键的话它们是不同键，下面的一方
 * provider 优先规则根本不触发，碰撞被推迟到后端才发生，而后端看不见 provider、
 * 只能听 HashMap 迭代顺序 —— 表现为面板每刷新一次总额就换一个值。
 */

const PRICING_URL = 'https://models.dev/api.json';
const CACHE_KEY = 'mini-term-model-pricing';
const CACHE_TTL_MS = 24 * 60 * 60 * 1000;
/** 缓存格式版本：建键规则变更时 +1。旧缓存不再当新鲜值用（仍可离线兜底） */
const CACHE_VERSION = 2;

/** 一方 provider：同一模型被多家登记时，官方目录的价格权威。
 *  随 grok 用量统计一并纳入 xai —— models.dev 上 `grok-*` 同样被几十家聚合商
 *  以各自的价登记，不加进来 grok 的成本会按某个随机聚合商的报价算。 */
const FIRST_PARTY_PROVIDERS = new Set(['anthropic', 'openai', 'xai']);
/** 模型 id 自带一方前缀的次优先（如 `anthropic/claude-opus-5` 来自聚合商目录） */
const FIRST_PARTY_HINTS = ['anthropic', 'openai', 'xai'];

interface CachedPricing {
  /** v1 缓存无此字段（按原始 modelId 建的键） */
  version?: number;
  fetchedAt: number;
  table: Record<string, ModelPriceEntry>;
}

/**
 * 模型名归一：小写、取 `/` 后段、剥 `@pin` 后缀、点转横线。
 * `anthropic/claude-opus-4.7` → `claude-opus-4-7`。
 *
 * 与 `src-tauri/src/usage_stats/pricing.rs` 的 `canonical()` 逐规则对齐（含先后
 * 顺序：先取 `/` 后段再剥 `@`）。两侧任一改动都必须同步，否则前端择优出来的
 * 键在后端会二次塌陷，碰撞择优白做。
 */
export function canonicalModelKey(name: string): string {
  let s = name.trim().toLowerCase();
  const slash = s.lastIndexOf('/');
  if (slash >= 0) s = s.slice(slash + 1);
  const at = s.indexOf('@');
  if (at >= 0) s = s.slice(0, at);
  return s.replace(/\./g, '-');
}

/** 同一 canonical 键下的候选来源（择优只看来源属性，不看价格高低） */
interface PricingCandidate {
  providerId: string;
  modelId: string;
  hasExplicitCacheRead: boolean;
  hasExplicitCacheWrite: boolean;
}

function providerPriority(candidate: PricingCandidate): number {
  if (FIRST_PARTY_PROVIDERS.has(candidate.providerId)) return 2;
  if (FIRST_PARTY_HINTS.some((hint) => candidate.modelId.includes(hint))) return 1;
  return 0;
}

/**
 * 候选择优的**全序**比较器（> 0 表示 left 胜出）。
 *
 * 关键性质是「任意两个候选都可比且不平局」：最后两级用 providerId / modelId
 * 字典序兜底，因此择优结果只由候选集合决定，与遍历顺序无关。
 */
function comparePricingCandidates(left: PricingCandidate, right: PricingCandidate): number {
  return (
    compareNumber(providerPriority(left), providerPriority(right)) ||
    compareBoolean(left.hasExplicitCacheRead, right.hasExplicitCacheRead) ||
    compareBoolean(left.hasExplicitCacheWrite, right.hasExplicitCacheWrite) ||
    comparePreferSmaller(left.providerId, right.providerId) ||
    comparePreferSmaller(left.modelId, right.modelId)
  );
}

function compareNumber(left: number, right: number): number {
  return left === right ? 0 : left > right ? 1 : -1;
}

function compareBoolean(left: boolean, right: boolean): number {
  return compareNumber(left ? 1 : 0, right ? 1 : 0);
}

function comparePreferSmaller(left: string, right: string): number {
  return left === right ? 0 : left < right ? 1 : -1;
}

/** 按键排序遍历：让「谁先被看到」也不再是 JS 对象插入顺序的函数 */
function sortedEntries<T>(value: Record<string, T>): Array<[string, T]> {
  return Object.entries(value).sort(([left], [right]) =>
    left < right ? -1 : left > right ? 1 : 0,
  );
}

/**
 * models.dev api.json → canonical 键的价格表。同键碰撞按
 * `comparePricingCandidates` 择优，全 0 占位价直接丢弃。
 *
 * 导出供 node 测试直接喂假 api.json（不经网络与 localStorage）。
 */
export function normalizePricingTable(api: unknown): Record<string, ModelPriceEntry> {
  if (typeof api !== 'object' || api === null) return {};

  const selected = new Map<string, { candidate: PricingCandidate; entry: ModelPriceEntry }>();
  const collided = new Set<string>();
  let collisionCount = 0;

  for (const [providerId, provider] of sortedEntries(api as Record<string, unknown>)) {
    const models = (provider as { models?: Record<string, unknown> })?.models;
    if (typeof models !== 'object' || models === null) continue;

    for (const [modelId, model] of sortedEntries(models)) {
      const cost = (model as { cost?: Record<string, number> })?.cost;
      if (typeof cost?.input !== 'number' || typeof cost?.output !== 'number') continue;
      // 全 0 占位价（部分订阅制/白名单 provider 用它表示「不单独计费」）：
      // 收下会把该模型整段成本抹成 0，比查不到价更糟 —— 查不到还有兜底均价
      if (cost.input === 0 && cost.output === 0) continue;

      const key = canonicalModelKey(modelId);
      if (!key) continue;

      const candidate: PricingCandidate = {
        providerId,
        modelId,
        hasExplicitCacheRead: typeof cost.cache_read === 'number',
        hasExplicitCacheWrite: typeof cost.cache_write === 'number',
      };
      const existing = selected.get(key);
      if (existing) {
        collided.add(key);
        collisionCount += 1;
        if (comparePricingCandidates(candidate, existing.candidate) <= 0) continue;
      }
      selected.set(key, {
        candidate,
        entry: {
          input: cost.input / 1e6,
          output: cost.output / 1e6,
          cacheRead: (cost.cache_read ?? 0) / 1e6,
          cacheWrite: (cost.cache_write ?? 0) / 1e6,
        },
      });
    }
  }

  if (collisionCount > 0) {
    // 不静默丢：碰撞本身是常态（一个模型被几十家登记），但数量突变往往意味着
    // 上游 id 规则变了，值得在控制台留痕以便对账
    console.warn(
      `[pricing] ${collided.size} 个模型键被多家 provider 重复登记（共 ${collisionCount} 条重复），已按一方 provider 优先择优`,
    );
  }

  return Object.fromEntries(Array.from(selected, ([key, picked]) => [key, picked.entry]));
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
  // 版本不符 = 建键规则已变（v1 缓存按原始 modelId 建键），不能当新鲜值直接用
  if (cached?.version === CACHE_VERSION && Date.now() - cached.fetchedAt < CACHE_TTL_MS) {
    return cached.table;
  }

  try {
    const resp = await fetch(PRICING_URL);
    if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
    const table = normalizePricingTable(await resp.json());
    if (Object.keys(table).length === 0) throw new Error('empty pricing table');
    try {
      localStorage.setItem(
        CACHE_KEY,
        JSON.stringify({ version: CACHE_VERSION, fetchedAt: Date.now(), table }),
      );
    } catch {
      /* 缓存写失败不影响本次使用 */
    }
    return table;
  } catch (e) {
    // 过期/旧版本缓存兜底：旧价也远好于无价。旧版缓存里的原始键会在后端二次
    // 塌陷，但后端对碰撞已是确定性择优，最坏是价格略偏，不会再来回跳
    if (cached) return cached.table;
    throw e;
  }
}
