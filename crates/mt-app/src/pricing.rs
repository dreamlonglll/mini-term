//! 模型价格表:拉 `https://models.dev/api.json` → 归一 → 24h 磁盘缓存。
//! 对应 `src/utils/modelPricing.ts`(206 行),逐规则移植。
//!
//! ```text
//! ensure_pricing(dir, now)
//!   ├─ 手工裸表 {dir}/model-pricing.json          ← 恒优先、无 TTL(用户自己放的)
//!   ├─ 新鲜缓存 {dir}/model-pricing.cache.json    ← version==2 且 24h 内
//!   ├─ 拉网 → normalize → 非空 → 写缓存
//!   └─ 过期/旧版缓存兜底 → 都没有才 Err
//! ```
//!
//! # 为什么建键要做「全序择优」
//!
//! models.dev 是 provider → models 两层结构,同一个模型会被几十家 provider 以
//! 不同 id、不同价登记(`claude-opus-5` / `anthropic/claude-opus-5` /
//! `claude-opus-5@eu` …)。按原始 modelId 建键的话它们是不同键,一方 provider
//! 优先的规则根本不触发,碰撞被推迟到 `mt_usage::PricingTable` 才发生,而那边
//! 看不见 provider、只能听 HashMap 迭代顺序 —— 表现为**面板每刷新一次总额就
//! 换一个值**。比较器最后两级的字典序兜底不是洁癖,是正确性:它保证「任意两个
//! 候选可比且不平局」,择优结果只由候选集合决定、与遍历顺序无关。
//!
//! # `input == 0 && output == 0` 必须丢
//!
//! 部分订阅制/白名单 provider 用全 0 表示「不单独计费」。收下会把该模型整段
//! 成本抹成 0,**比查不到价更糟** —— 查不到还有 `PricingTable` 的三锚点兜底均价。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use mt_usage::ModelPrice;
use serde::{Deserialize, Serialize};

/// 归一后的价格表:canonical 模型名 → $/token。
pub type PricingMap = HashMap<String, ModelPrice>;

const PRICING_URL: &str = "https://models.dev/api.json";
/// 缓存格式版本:**建键规则变更时 +1**。版本不符不当新鲜值(仍可离线兜底)。
const CACHE_VERSION: u32 = 2;
/// 24h,与 `modelPricing.ts:21` 的 `CACHE_TTL_MS` 同。
const CACHE_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// 用户手放的裸表(`usageStats.pricingLocalHint` 那条文案指的就是它)。
const MANUAL_FILE: &str = "model-pricing.json";
/// 本程序写的网络缓存。**与手工表分文件** —— 不能把用户手放的表在第一次
/// 成功拉网后悄悄覆盖掉。
const CACHE_FILE: &str = "model-pricing.cache.json";

/// 一方 provider:同一模型被多家登记时,官方目录的价格权威。
/// (grok 同样被几十家聚合商以各自的价登记,不加 `xai` 的话成本会按某个
/// 随机聚合商的报价算。)
const FIRST_PARTY_PROVIDERS: [&str; 3] = ["anthropic", "openai", "xai"];
/// 模型 id 自带一方前缀的次优先(如 `anthropic/claude-opus-5` 来自聚合商目录)。
const FIRST_PARTY_HINTS: [&str; 3] = ["anthropic", "openai", "xai"];

// ─── 建键与择优(纯函数) ──────────────────────────────────────

/// 模型名归一:小写、取 `/` 后段、剥 `@pin` 后缀、点转横线。
/// `anthropic/claude-opus-4.7` → `claude-opus-4-7`。
///
/// 与 `mt_usage::pricing::canonical()` **逐规则对齐**(含先后顺序:
/// 先取 `/` 后段再剥 `@`)。两侧任一改动都必须同步,否则这里择优出来的键
/// 在账本侧会二次塌陷,择优白做。
pub fn canonical_model_key(name: &str) -> String {
    let mut s = name.trim().to_lowercase();
    if let Some(idx) = s.rfind('/') {
        s = s[idx + 1..].to_string();
    }
    if let Some(idx) = s.find('@') {
        s.truncate(idx);
    }
    s.replace('.', "-")
}

/// 同一 canonical 键下的候选来源(择优**只看来源属性**,不看价格高低 ——
/// 挑贵的或挑便宜的都是在替用户做经济判断)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PricingCandidate {
    pub provider_id: String,
    pub model_id: String,
    pub has_explicit_cache_read: bool,
    pub has_explicit_cache_write: bool,
}

fn provider_priority(c: &PricingCandidate) -> u8 {
    if FIRST_PARTY_PROVIDERS.contains(&c.provider_id.as_str()) {
        return 2;
    }
    if FIRST_PARTY_HINTS
        .iter()
        .any(|hint| c.model_id.contains(hint))
    {
        return 1;
    }
    0
}

/// 候选择优的**全序**比较器(`Greater` = left 胜出)。
///
/// 依次比:① 一方 provider → ② modelId 含一方前缀 → ③ 有显式 `cache_read`
/// → ④ 有显式 `cache_write` → ⑤ providerId 字典序**小者胜** → ⑥ modelId 同理。
/// 最后两级是为了「不平局」。
pub fn compare_pricing_candidates(
    left: &PricingCandidate,
    right: &PricingCandidate,
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    provider_priority(left)
        .cmp(&provider_priority(right))
        .then_with(|| {
            left.has_explicit_cache_read
                .cmp(&right.has_explicit_cache_read)
        })
        .then_with(|| {
            left.has_explicit_cache_write
                .cmp(&right.has_explicit_cache_write)
        })
        // 字典序**小者胜** —— 所以两边反过来比
        .then_with(|| match left.provider_id.cmp(&right.provider_id) {
            Ordering::Less => Ordering::Greater,
            Ordering::Greater => Ordering::Less,
            Ordering::Equal => Ordering::Equal,
        })
        .then_with(|| match left.model_id.cmp(&right.model_id) {
            Ordering::Less => Ordering::Greater,
            Ordering::Greater => Ordering::Less,
            Ordering::Equal => Ordering::Equal,
        })
}

/// models.dev api.json → canonical 键的价格表($/token,四个值全部 ÷1e6)。
///
/// 遍历本身也按键排序 —— 让「谁先被看到」不再是 map 迭代顺序的函数
/// (虽然有全序比较器兜底,排序遍历让日志里的碰撞计数也稳定)。
pub fn normalize_pricing_table(api: &serde_json::Value) -> PricingMap {
    let Some(providers) = api.as_object() else {
        return PricingMap::new();
    };

    let mut selected: HashMap<String, (PricingCandidate, ModelPrice)> = HashMap::new();
    let mut collided: std::collections::HashSet<String> = Default::default();
    let mut collision_count = 0usize;

    let mut provider_ids: Vec<&String> = providers.keys().collect();
    provider_ids.sort_unstable();

    for provider_id in provider_ids {
        let Some(models) = providers[provider_id].get("models").and_then(|m| m.as_object()) else {
            continue;
        };
        let mut model_ids: Vec<&String> = models.keys().collect();
        model_ids.sort_unstable();

        for model_id in model_ids {
            let Some(cost) = models[model_id].get("cost") else {
                continue;
            };
            let (Some(input), Some(output)) = (
                cost.get("input").and_then(|v| v.as_f64()),
                cost.get("output").and_then(|v| v.as_f64()),
            ) else {
                continue;
            };
            // 全 0 占位价:收下比查不到更糟(查不到还有兜底均价)
            if input == 0.0 && output == 0.0 {
                continue;
            }
            let key = canonical_model_key(model_id);
            if key.is_empty() {
                continue;
            }

            let cache_read = cost.get("cache_read").and_then(|v| v.as_f64());
            let cache_write = cost.get("cache_write").and_then(|v| v.as_f64());
            let candidate = PricingCandidate {
                provider_id: provider_id.clone(),
                model_id: model_id.clone(),
                has_explicit_cache_read: cache_read.is_some(),
                has_explicit_cache_write: cache_write.is_some(),
            };
            if let Some((existing, _)) = selected.get(&key) {
                collided.insert(key.clone());
                collision_count += 1;
                if compare_pricing_candidates(&candidate, existing) != std::cmp::Ordering::Greater {
                    continue;
                }
            }
            selected.insert(
                key,
                (
                    candidate,
                    ModelPrice {
                        input: input / 1e6,
                        output: output / 1e6,
                        cache_read: cache_read.unwrap_or(0.0) / 1e6,
                        cache_write: cache_write.unwrap_or(0.0) / 1e6,
                    },
                ),
            );
        }
    }

    if collision_count > 0 {
        // 不静默丢:碰撞本身是常态(一个模型被几十家登记),但数量突变往往
        // 意味着上游 id 规则变了,值得留痕以便对账
        eprintln!(
            "[pricing] {} 个模型键被多家 provider 重复登记(共 {collision_count} 条重复),已按一方 provider 优先择优",
            collided.len()
        );
    }

    selected.into_iter().map(|(k, (_, p))| (k, p)).collect()
}

// ─── 磁盘信封 ────────────────────────────────────────────────

/// 可序列化的单模型价格。`mt_usage::ModelPrice` 只 derive 了 `Deserialize`
/// (它是「前端传进来」的入参类型),写盘需要自己这一份同构镜像。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct PriceEntry {
    #[serde(default)]
    input: f64,
    #[serde(default)]
    output: f64,
    #[serde(default)]
    cache_read: f64,
    #[serde(default)]
    cache_write: f64,
}

impl From<&ModelPrice> for PriceEntry {
    fn from(p: &ModelPrice) -> Self {
        Self {
            input: p.input,
            output: p.output,
            cache_read: p.cache_read,
            cache_write: p.cache_write,
        }
    }
}

impl From<PriceEntry> for ModelPrice {
    fn from(p: PriceEntry) -> Self {
        Self {
            input: p.input,
            output: p.output,
            cache_read: p.cache_read,
            cache_write: p.cache_write,
        }
    }
}

/// 磁盘缓存的信封,与旧版 localStorage 的结构同构。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingCache {
    #[serde(default)]
    pub version: u32,
    pub fetched_at: i64,
    /// 私有 —— `PriceEntry` 是本模块的序列化镜像,不该漏进模块外的签名里。
    /// 外部走 [`PricingCache::to_map`]。
    table: HashMap<String, PriceEntry>,
}

impl PricingCache {
    /// 版本相符且在 TTL 内 —— 只有这种才当「新鲜」直接用。
    pub fn is_fresh(&self, now_ms: i64) -> bool {
        self.version == CACHE_VERSION && now_ms.saturating_sub(self.fetched_at) < CACHE_TTL_MS
    }

    pub fn to_map(&self) -> PricingMap {
        self.table
            .iter()
            .map(|(k, v)| (k.clone(), ModelPrice::from(*v)))
            .collect()
    }
}

/// 这一份表从哪来的(UI 只用它决定要不要提示,不参与计价)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PricingSource {
    /// 用户手放的裸表。
    Manual,
    /// 新鲜缓存(没走网络)。
    FreshCache,
    /// 本次拉网拿到的。
    Network,
    /// 拉网失败,用过期/旧版缓存兜底。
    StaleCache,
}

fn manual_path(dir: &Path) -> PathBuf {
    dir.join(MANUAL_FILE)
}
fn cache_path(dir: &Path) -> PathBuf {
    dir.join(CACHE_FILE)
}

/// 用户手放的**裸表**(`{"claude-opus-4-8": {"input": 1.5e-5, …}}`,$/token)。
///
/// 同一个文件如果是本程序写的信封格式,这里返回 `None`(交给 [`read_cache`])。
/// 空表也当没有 —— 空文件不该把整机成本按 $0 算。
pub fn load_manual_pricing(dir: &Path) -> Option<PricingMap> {
    let text = std::fs::read_to_string(manual_path(dir)).ok()?;
    let table: HashMap<String, PriceEntry> = serde_json::from_str(&text).ok()?;
    if table.is_empty() {
        return None;
    }
    Some(
        table
            .into_iter()
            .map(|(k, v)| (k, ModelPrice::from(v)))
            .collect(),
    )
}

/// 本程序写的缓存信封。旧格式(`model-pricing.json` 里放着信封)一并认。
pub fn read_cache(dir: &Path) -> Option<PricingCache> {
    for path in [cache_path(dir), manual_path(dir)] {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(cache) = serde_json::from_str::<PricingCache>(&text)
            && !cache.table.is_empty()
        {
            return Some(cache);
        }
    }
    None
}

fn write_cache(dir: &Path, table: &PricingMap, now_ms: i64) {
    let cache = PricingCache {
        version: CACHE_VERSION,
        fetched_at: now_ms,
        table: table.iter().map(|(k, v)| (k.clone(), v.into())).collect(),
    };
    // 缓存写失败不影响本次使用(与旧版 localStorage 那个空 catch 同)
    if let Ok(text) = serde_json::to_string(&cache) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(cache_path(dir), text);
    }
}

// ─── 降级链路(纯逻辑,可单测) ────────────────────────────────

/// 手工表 / 缓存 / 拉网三个候选选谁。**`fetch` 是闭包** —— 单测直接喂桩,
/// 网络层本身不测。
///
/// 顺序照 `modelPricing.ts:180-205` + `useUsageStats.ts:113-127`:
/// 手工表 → 新鲜缓存 → 拉网成功 → 过期/旧版缓存 → 报错。
///
/// 注意**新鲜缓存命中也不绕过下一轮 TTL**:应用常驻多日后过期价格照常重拉
/// (旧版 `:109-111` 的注释专门写了这条)。
pub fn resolve_pricing(
    manual: Option<PricingMap>,
    cache: Option<PricingCache>,
    now_ms: i64,
    fetch: impl FnOnce() -> Result<PricingMap, String>,
) -> Result<(PricingMap, PricingSource), String> {
    if let Some(table) = manual {
        return Ok((table, PricingSource::Manual));
    }
    if let Some(cache) = &cache
        && cache.is_fresh(now_ms)
    {
        return Ok((cache.to_map(), PricingSource::FreshCache));
    }
    match fetch() {
        Ok(table) if !table.is_empty() => Ok((table, PricingSource::Network)),
        Ok(_) => match cache {
            // 归一后 0 条与 HTTP 失败同等对待(旧版 `:190` 是 throw)
            Some(cache) => Ok((cache.to_map(), PricingSource::StaleCache)),
            None => Err("empty pricing table".to_string()),
        },
        Err(err) => match cache {
            // 旧价也远好于无价
            Some(cache) => Ok((cache.to_map(), PricingSource::StaleCache)),
            None => Err(err),
        },
    }
}

// ─── 网络与装配(阻塞,调用方必须丢 background executor) ──────

/// 一次 HTTPS GET + 归一。**阻塞**:DNS + TLS 握手动辄几百 ms,绝不能落主线程。
///
/// 无自定义头(旧版就是裸 `fetch(PRICING_URL)`),15s 超时是 GPUI 侧新加的
/// ——浏览器有自己的超时,`reqwest::blocking` 默认无限等。
fn fetch_models_dev() -> Result<PricingMap, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(PRICING_URL).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status().as_u16()));
    }
    // 自己解析而不是 `resp.json()`:那个方法要 reqwest 的 `json` feature,
    // 而它会把 serde_json 拉进 reqwest 自己的 feature 面 —— 没必要
    let text = resp.text().map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    Ok(normalize_pricing_table(&json))
}

/// 取一份可用的价格表。**整体阻塞**,调用方丢 `cx.background_executor()`。
///
/// 拉网成功时顺带写缓存;手工表 / 新鲜缓存命中即瞬时返回,不碰网络。
pub fn ensure_pricing(dir: &Path, now_ms: i64) -> Result<(PricingMap, PricingSource), String> {
    let manual = load_manual_pricing(dir);
    let cache = read_cache(dir);
    let result = resolve_pricing(manual, cache, now_ms, fetch_models_dev)?;
    if result.1 == PricingSource::Network {
        write_cache(dir, &result.0, now_ms);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn candidate(provider: &str, model: &str, read: bool, write: bool) -> PricingCandidate {
        PricingCandidate {
            provider_id: provider.into(),
            model_id: model.into(),
            has_explicit_cache_read: read,
            has_explicit_cache_write: write,
        }
    }

    /// 归一顺序要紧:**先取 `/` 后段再剥 `@`**,最后点转横线。
    /// 与 `mt_usage::pricing::canonical()` 逐规则对齐。
    #[test]
    fn canonical_键与账本侧同规则() {
        assert_eq!(
            canonical_model_key("anthropic/claude-opus-4.7"),
            "claude-opus-4-7"
        );
        assert_eq!(canonical_model_key("  Claude-Opus-5  "), "claude-opus-5");
        assert_eq!(canonical_model_key("claude-opus-5@eu"), "claude-opus-5");
        // 先取 `/` 后段:`@` 在前段里的话不该被剥
        assert_eq!(canonical_model_key("a@b/claude-opus-5"), "claude-opus-5");
        assert_eq!(canonical_model_key("openai/gpt-5.3@2026"), "gpt-5-3");
        assert_eq!(canonical_model_key(""), "");
    }

    /// 比较器是**全序**:任意两个不同候选都能分出胜负(不平局)。
    #[test]
    fn 择优比较器不平局() {
        use std::cmp::Ordering;
        let first_party = candidate("anthropic", "claude-opus-5", false, false);
        let aggregator = candidate("zzz-hub", "claude-opus-5", true, true);
        assert_eq!(
            compare_pricing_candidates(&first_party, &aggregator),
            Ordering::Greater,
            "一方 provider 压过「缓存字段更全」"
        );

        // 次优先:modelId 自带一方前缀
        let hinted = candidate("hub", "anthropic/claude-opus-5", false, false);
        let plain = candidate("hub2", "claude-opus-5", false, false);
        assert_eq!(
            compare_pricing_candidates(&hinted, &plain),
            Ordering::Greater
        );

        // 同优先级 → 有显式 cache_read 胜
        let with_read = candidate("hub", "m", true, false);
        let without = candidate("hub", "m", false, true);
        assert_eq!(
            compare_pricing_candidates(&with_read, &without),
            Ordering::Greater
        );

        // 全平 → providerId 字典序小者胜
        assert_eq!(
            compare_pricing_candidates(&candidate("aaa", "m", false, false), &candidate("bbb", "m", false, false)),
            Ordering::Greater
        );
        // provider 也相同 → modelId 字典序小者胜
        assert_eq!(
            compare_pricing_candidates(&candidate("p", "aaa", false, false), &candidate("p", "bbb", false, false)),
            Ordering::Greater
        );
        // 完全相同才允许平局
        assert_eq!(
            compare_pricing_candidates(&candidate("p", "m", false, false), &candidate("p", "m", false, false)),
            Ordering::Equal
        );
    }

    /// 择优结果只由候选集合决定,与遍历顺序无关 —— 这条不成立的症状是
    /// 「面板每刷新一次总额就换一个值」。
    #[test]
    fn 择优与遍历顺序无关() {
        let a = json!({
            "zzz-hub":   { "models": { "claude-opus-5": { "cost": { "input": 99.0, "output": 99.0 } } } },
            "anthropic": { "models": { "claude-opus-5": { "cost": { "input": 5.0,  "output": 25.0 } } } },
        });
        let b = json!({
            "anthropic": { "models": { "claude-opus-5": { "cost": { "input": 5.0,  "output": 25.0 } } } },
            "zzz-hub":   { "models": { "claude-opus-5": { "cost": { "input": 99.0, "output": 99.0 } } } },
        });
        let ta = normalize_pricing_table(&a);
        let tb = normalize_pricing_table(&b);
        assert_eq!(ta, tb);
        let price = ta.get("claude-opus-5").unwrap();
        assert!((price.input - 5e-6).abs() < 1e-15, "取一方 provider 的价");
    }

    /// 单位换算:四个值全部 ÷1e6($/1M → $/token)。
    #[test]
    fn 价格按百万分之一换算() {
        let api = json!({
            "anthropic": { "models": { "claude-opus-5": {
                "cost": { "input": 5.0, "output": 25.0, "cache_read": 0.5, "cache_write": 6.25 }
            } } }
        });
        let table = normalize_pricing_table(&api);
        let p = table.get("claude-opus-5").unwrap();
        assert!((p.input - 5e-6).abs() < 1e-15);
        assert!((p.output - 25e-6).abs() < 1e-15);
        assert!((p.cache_read - 0.5e-6).abs() < 1e-15);
        assert!((p.cache_write - 6.25e-6).abs() < 1e-15);
    }

    /// `input == 0 && output == 0` 的占位价必须丢:收下会把该模型整段成本
    /// 抹成 0,比查不到价更糟(查不到还有兜底均价)。
    #[test]
    fn 全零占位价丢弃() {
        let api = json!({
            "subs": { "models": {
                "free-model":  { "cost": { "input": 0.0, "output": 0.0 } },
                "half-free":   { "cost": { "input": 0.0, "output": 3.0 } },
                "no-cost":     { },
                "bad-cost":    { "cost": { "input": "x", "output": 1.0 } },
            } }
        });
        let table = normalize_pricing_table(&api);
        assert!(!table.contains_key("free-model"), "全 0 占位价必须丢");
        assert!(table.contains_key("half-free"), "只有一侧为 0 是真价");
        assert!(!table.contains_key("no-cost"));
        assert!(!table.contains_key("bad-cost"), "非 number 跳过该模型");
    }

    /// 顶层不是对象 / models 不是对象 → 空表而不是 panic。
    #[test]
    fn 坏响应归一成空表() {
        assert!(normalize_pricing_table(&json!(null)).is_empty());
        assert!(normalize_pricing_table(&json!([1, 2])).is_empty());
        assert!(normalize_pricing_table(&json!({"p": {"models": 3}})).is_empty());
    }

    fn table_of(key: &str, input: f64) -> PricingMap {
        let mut m = PricingMap::new();
        m.insert(
            key.to_string(),
            ModelPrice {
                input,
                ..Default::default()
            },
        );
        m
    }

    fn cache_of(key: &str, input: f64, version: u32, fetched_at: i64) -> PricingCache {
        PricingCache {
            version,
            fetched_at,
            table: [(
                key.to_string(),
                PriceEntry {
                    input,
                    ..Default::default()
                },
            )]
            .into_iter()
            .collect(),
        }
    }

    /// 降级链路:手工表 → 新鲜缓存 → 拉网 → 过期缓存 → 报错。
    #[test]
    fn 价格降级链路按优先级() {
        let now = 1_000_000_000i64;

        // ① 手工表恒优先,连网络都不碰
        let (t, src) = resolve_pricing(
            Some(table_of("manual", 1.0)),
            Some(cache_of("cache", 2.0, CACHE_VERSION, now)),
            now,
            || panic!("手工表在场时不许拉网"),
        )
        .unwrap();
        assert_eq!(src, PricingSource::Manual);
        assert!(t.contains_key("manual"));

        // ② 新鲜缓存(版本相符 + 24h 内)
        let (t, src) = resolve_pricing(
            None,
            Some(cache_of("cache", 2.0, CACHE_VERSION, now - 1000)),
            now,
            || panic!("新鲜缓存命中时不许拉网"),
        )
        .unwrap();
        assert_eq!(src, PricingSource::FreshCache);
        assert!(t.contains_key("cache"));

        // ③ 缓存过期 → 拉网
        let (t, src) = resolve_pricing(
            None,
            Some(cache_of("cache", 2.0, CACHE_VERSION, now - CACHE_TTL_MS - 1)),
            now,
            || Ok(table_of("net", 3.0)),
        )
        .unwrap();
        assert_eq!(src, PricingSource::Network);
        assert!(t.contains_key("net"));

        // ④ 拉网失败 → 过期缓存兜底(旧价远好于无价)
        let (t, src) = resolve_pricing(
            None,
            Some(cache_of("cache", 2.0, CACHE_VERSION, now - CACHE_TTL_MS - 1)),
            now,
            || Err("HTTP 503".into()),
        )
        .unwrap();
        assert_eq!(src, PricingSource::StaleCache);
        assert!(t.contains_key("cache"));

        // ⑤ 什么都没有 → 报错(**绝不返回空表当真数据**)
        assert_eq!(
            resolve_pricing(None, None, now, || Err("HTTP 503".into())).unwrap_err(),
            "HTTP 503"
        );
        // 归一后 0 条与 HTTP 失败同等对待
        assert_eq!(
            resolve_pricing(None, None, now, || Ok(PricingMap::new())).unwrap_err(),
            "empty pricing table"
        );
    }

    /// 版本不符的缓存**不当新鲜值**(建键规则已变),但仍可离线兜底。
    #[test]
    fn 旧版本缓存不当新鲜值但可兜底() {
        let now = 1_000_000_000i64;
        let old = cache_of("v1", 2.0, CACHE_VERSION - 1, now);
        assert!(!old.is_fresh(now));

        let (t, src) = resolve_pricing(None, Some(old.clone()), now, || Ok(table_of("net", 3.0))).unwrap();
        assert_eq!(src, PricingSource::Network, "版本不符要重拉");
        assert!(t.contains_key("net"));

        let (t, src) = resolve_pricing(None, Some(old), now, || Err("offline".into())).unwrap();
        assert_eq!(src, PricingSource::StaleCache, "拉不到时旧版缓存照样兜底");
        assert!(t.contains_key("v1"));
    }

    /// 手工裸表与信封缓存分文件存放 —— 拉网成功不许覆盖用户手放的那份。
    #[test]
    fn 手工裸表与缓存信封各读各的() {
        let dir = std::env::temp_dir().join("mt-app-pricing-split-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 空目录:两边都拿不到
        assert!(load_manual_pricing(&dir).is_none());
        assert!(read_cache(&dir).is_none());

        std::fs::write(
            dir.join(MANUAL_FILE),
            r#"{"claude-opus-5":{"input":1.5e-5,"output":7.5e-5}}"#,
        )
        .unwrap();
        let manual = load_manual_pricing(&dir).expect("裸表要读得出来");
        assert!((manual["claude-opus-5"].input - 1.5e-5).abs() < 1e-15);
        assert!(read_cache(&dir).is_none(), "裸表不是信封,不该被当缓存");

        write_cache(&dir, &table_of("net-model", 9.0), 42);
        let cache = read_cache(&dir).expect("信封要读得出来");
        assert_eq!(cache.version, CACHE_VERSION);
        assert_eq!(cache.fetched_at, 42);
        assert!(cache.to_map().contains_key("net-model"));
        // 手工表原样还在
        assert!(load_manual_pricing(&dir).unwrap().contains_key("claude-opus-5"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空的手工表当没有 —— 空文件不该把整机成本按 $0 算。
    #[test]
    fn 空手工表当没有() {
        let dir = std::env::temp_dir().join("mt-app-pricing-empty-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANUAL_FILE), "{}").unwrap();
        assert!(load_manual_pricing(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
