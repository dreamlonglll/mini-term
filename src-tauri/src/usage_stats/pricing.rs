use serde::Deserialize;
use std::cmp::Reverse;
use std::collections::HashMap;

use super::turns::UsageTotals;

/// 单模型价格（$/token，前端拉 models.dev 后已 ÷1e6 换算）。
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPrice {
    #[serde(default)]
    pub input: f64,
    #[serde(default)]
    pub output: f64,
    #[serde(default)]
    pub cache_read: f64,
    #[serde(default)]
    pub cache_write: f64,
}

/// 查价表：前端传入的模型价格 map + 三锚点均价兜底。
pub struct PricingTable {
    exact: HashMap<String, ModelPrice>,
    fallback: Option<ModelPrice>,
}

/// 兜底锚点：查价全失败时取三者均价（表为空才彻底记 $0）。
const ANCHOR_MODELS: [&str; 3] = ["claude-sonnet-4-6", "claude-opus-4-7", "claude-opus-4-8"];

/// 模型名归一：小写、取 `/` 后段、剥 `@pin` 后缀、点转横线。
/// `anthropic/claude-opus-4.7` → `claude-opus-4-7`。
/// pub(super)：聚合层的按模型分组复用同一归一规则。
pub(super) fn canonical(name: &str) -> String {
    let mut s = name.trim().to_lowercase();
    if let Some(idx) = s.rfind('/') {
        s = s[idx + 1..].to_string();
    }
    if let Some(idx) = s.find('@') {
        s.truncate(idx);
    }
    s.replace('.', "-")
}

/// 剥尾部日期后缀（`claude-sonnet-4-5-20250929` → `claude-sonnet-4-5`）。
pub(super) fn strip_date_suffix(name: &str) -> Option<&str> {
    let (head, tail) = name.rsplit_once('-')?;
    if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
        Some(head)
    } else {
        None
    }
}

/// 同一 canonical 键下的择优序（元组字典序，越大越优先）。
///
/// 只看「来源形态」与「价格是否完整」，不比价格高低——挑贵的或挑便宜的都是在
/// 替用户做经济判断，而挑「官方裸模型名 + 缓存单价齐全」的那条是可辩护的。
fn candidate_rank(raw_key: &str, p: &ModelPrice) -> (bool, bool, bool, bool, bool, Reverse<usize>) {
    (
        // 全 0 占位价排最后：收下会把该模型整段成本抹成 0，比查不到价更糟
        p.input > 0.0 || p.output > 0.0,
        // 显式给出缓存单价者优先（缺失会退化成 0，Claude 侧缓存占大头，误差极大）
        p.cache_read > 0.0,
        p.cache_write > 0.0,
        // 裸模型名优于 provider 前缀形式（`anthropic/claude-opus-5`）
        !raw_key.contains('/'),
        // 裸模型名优于地区/日期后缀（`claude-opus-5@eu`、`claude-opus-5@20251101`）
        !raw_key.contains('@'),
        Reverse(raw_key.len()),
    )
}

/// 把原始价格表按 canonical 键压平，碰撞时按 `candidate_rank` 全序择优。
///
/// 不能直接 `for (k, v) in raw { exact.insert(canonical(&k), v) }`：models.dev
/// 同一模型会被几十家 provider 以不同 id、不同价登记（`claude-opus-5` /
/// `anthropic/claude-opus-5` / `claude-opus-5@eu` 全部塌成同一个键），而 std
/// HashMap 的迭代顺序**逐实例**随机（RandomState 每 new 一个 map 就换种子），
/// 于是「最后写入者胜出」每次查询都换人——表现为面板每点一次刷新，总额就在
/// 几个值之间来回跳（实测 `claude-opus-5` 的官方价与 requesty 欧区价正好差
/// 1.10 倍，同一天用量因此在 $10.02 / $11.02 之间反复横跳）。
///
/// 排序键完全不依赖迭代顺序，且末位用原始键字典序兜底，构成全序：同样的输入
/// 永远得到同样的表。前端 `modelPricing.ts` 已按一方 provider 择优过一轮，这里
/// 是第二道闸——价格表跨 IPC 传入是任意内容，且旧版 localStorage 缓存仍是按
/// 原始 modelId 建的键，不能假设无碰撞。
fn dedupe_by_canonical(raw: HashMap<String, ModelPrice>) -> Vec<(String, ModelPrice)> {
    let mut entries: Vec<(String, String, ModelPrice)> = raw
        .into_iter()
        .map(|(key, price)| (canonical(&key), key, price))
        .collect();
    entries.sort_by(|(a_key, a_raw, a_price), (b_key, b_raw, b_price)| {
        a_key
            .cmp(b_key)
            .then_with(|| candidate_rank(b_raw, b_price).cmp(&candidate_rank(a_raw, a_price)))
            .then_with(|| a_raw.cmp(b_raw))
    });
    let mut out: Vec<(String, ModelPrice)> = Vec::with_capacity(entries.len());
    for (key, _, price) in entries {
        // 同 canonical 键已排在一起，组内第一个即最优，其余丢弃
        if out.last().is_some_and(|(prev, _)| *prev == key) {
            continue;
        }
        out.push((key, price));
    }
    out
}

impl PricingTable {
    pub fn new(raw: HashMap<String, ModelPrice>) -> Self {
        let mut exact = HashMap::with_capacity(raw.len());
        for (key, price) in dedupe_by_canonical(raw) {
            exact.insert(key, price);
        }
        let anchors: Vec<ModelPrice> = ANCHOR_MODELS
            .iter()
            .filter_map(|m| exact.get(*m).copied())
            .collect();
        let fallback = if anchors.is_empty() {
            None
        } else {
            let n = anchors.len() as f64;
            Some(ModelPrice {
                input: anchors.iter().map(|p| p.input).sum::<f64>() / n,
                output: anchors.iter().map(|p| p.output).sum::<f64>() / n,
                cache_read: anchors.iter().map(|p| p.cache_read).sum::<f64>() / n,
                cache_write: anchors.iter().map(|p| p.cache_write).sum::<f64>() / n,
            })
        };
        Self { exact, fallback }
    }

    /// 查价链：canonical 精确 → 剥日期后缀 → 最长前缀 → 锚点均价。
    /// 前缀匹配要求断点落在 `-` 上（`gpt-5-mini` 不塌到 `gpt-5`，因为精确键先命中；
    /// 未知新款 `claude-opus-4-9` 可塌到表内的 `claude-opus-4` 系列键）。
    pub fn resolve(&self, model: &str) -> Option<ModelPrice> {
        let c = canonical(model);
        if c.is_empty() || c == "<synthetic>" {
            return None;
        }
        if let Some(p) = self.exact.get(&c) {
            return Some(*p);
        }
        if let Some(stripped) = strip_date_suffix(&c) {
            if let Some(p) = self.exact.get(stripped) {
                return Some(*p);
            }
        }
        let mut best: Option<(&String, &ModelPrice)> = None;
        for (k, v) in &self.exact {
            let boundary_ok = c.starts_with(k.as_str())
                && (c.len() == k.len() || c.as_bytes()[k.len()] == b'-');
            // 最长前缀优先，等长按字典序最小兜底。等长前缀理论上唯一（两个不同
            // 的等长字符串不可能同为一个串的前缀），兜底是为了让「与 HashMap
            // 迭代顺序无关」成为显式不变量，而不是依赖这条推理成立
            let better = |(bk, _): (&String, &ModelPrice)| {
                k.len() > bk.len() || (k.len() == bk.len() && k < bk)
            };
            if boundary_ok && best.is_none_or(better) {
                best = Some((k, v));
            }
        }
        if let Some((_, p)) = best {
            return Some(*p);
        }
        self.fallback
    }
}

/// 成本公式（口径见 docs/plans/2026-08-01-usage-stats-design.md §6.4）：
/// 1h 缓存写单价 = 5m 档 ×1.6，`cache_write` 已含 1h 子集，故子集只补 0.6 倍差价；
/// reasoning 按 output 单价（Codex 单列，Claude 恒 0，不会双扣）。
pub fn cost_of(u: &UsageTotals, p: &ModelPrice) -> f64 {
    u.input as f64 * p.input
        + (u.output + u.reasoning) as f64 * p.output
        + u.cache_write as f64 * p.cache_write
        + u.cache_write_1h as f64 * p.cache_write * 0.6
        + u.cache_read as f64 * p.cache_read
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(entries: &[(&str, f64)]) -> PricingTable {
        let raw = entries
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    ModelPrice {
                        input: *v,
                        output: *v * 5.0,
                        cache_read: *v * 0.1,
                        cache_write: *v * 1.25,
                    },
                )
            })
            .collect();
        PricingTable::new(raw)
    }

    #[test]
    fn canonical_normalizes_provider_prefix_dots_and_pin() {
        assert_eq!(canonical("anthropic/claude-opus-4.7"), "claude-opus-4-7");
        assert_eq!(canonical("Claude-Opus-4-8"), "claude-opus-4-8");
        assert_eq!(canonical("gpt-5.3-codex@pin"), "gpt-5-3-codex");
    }

    #[test]
    fn resolve_exact_then_date_suffix() {
        let t = table(&[("claude-sonnet-4-5", 3e-6)]);
        assert!(t.resolve("claude-sonnet-4-5").is_some());
        // 剥日期后缀
        assert!(t.resolve("claude-sonnet-4-5-20250929").is_some());
    }

    #[test]
    fn resolve_prefix_does_not_collapse_specific_to_generic() {
        let t = table(&[("gpt-5", 1e-6), ("gpt-5-mini", 2e-7)]);
        // 精确命中 mini，不塌到 gpt-5
        assert_eq!(t.resolve("gpt-5-mini").unwrap().input, 2e-7);
        // 未知新款按最长前缀塌到系列
        assert_eq!(t.resolve("gpt-5-mini-turbo").unwrap().input, 2e-7);
        // 断点不在 `-` 上不算前缀（gpt-52 不该命中 gpt-5）
        assert!(t.resolve("gpt-52").is_none() || t.resolve("gpt-52").unwrap().input != 1e-6);
    }

    #[test]
    fn resolve_falls_back_to_anchor_average() {
        let t = table(&[("claude-opus-4-7", 10e-6), ("claude-opus-4-8", 20e-6)]);
        let p = t.resolve("totally-unknown-model").unwrap();
        assert!((p.input - 15e-6).abs() < 1e-12);
    }

    #[test]
    fn resolve_empty_table_returns_none() {
        let t = PricingTable::new(HashMap::new());
        assert!(t.resolve("claude-opus-4-8").is_none());
    }

    fn price(input: f64, output: f64, cache_read: f64, cache_write: f64) -> ModelPrice {
        ModelPrice { input, output, cache_read, cache_write }
    }

    /// models.dev 现网数据的真实碰撞组：四个原始键全部塌成 `claude-opus-5`，
    /// requesty 欧区价恰是官方价的 1.10 倍。
    fn colliding_opus5_table() -> HashMap<String, ModelPrice> {
        let mut m = HashMap::new();
        m.insert("claude-opus-5".to_string(), price(5e-6, 25e-6, 0.5e-6, 6.25e-6));
        m.insert("anthropic/claude-opus-5".to_string(), price(5e-6, 25e-6, 0.5e-6, 6.25e-6));
        m.insert("claude-opus-5@default".to_string(), price(5e-6, 25e-6, 0.5e-6, 6.25e-6));
        m.insert("claude-opus-5@eu".to_string(), price(5.5e-6, 27.5e-6, 0.55e-6, 6.88e-6));
        m
    }

    /// 回归：同一份价格表构造多次，查价结果必须逐次相同。
    ///
    /// 修复前 `PricingTable::new` 直接 `insert(canonical(k), v)`，胜出者由 std
    /// HashMap 的迭代顺序决定；RandomState 每 new 一个 map 就换种子，于是每次
    /// 查询都重新掷骰子——面板每点一次刷新总额就在 $10.02 / $11.02 间跳。
    /// 单轮断言必然通过，唯有多轮才能抓到，故这里跑 64 轮。
    #[test]
    fn canonical_collisions_resolve_deterministically() {
        let expected = PricingTable::new(colliding_opus5_table())
            .resolve("claude-opus-5")
            .expect("碰撞组必须解析出价格");
        for round in 0..64 {
            let got = PricingTable::new(colliding_opus5_table())
                .resolve("claude-opus-5")
                .expect("碰撞组必须解析出价格");
            assert_eq!(got, expected, "第 {round} 轮查价与首轮不一致：择优仍依赖迭代顺序");
        }
    }

    #[test]
    fn bare_official_model_name_wins_collision() {
        let t = PricingTable::new(colliding_opus5_table());
        let p = t.resolve("claude-opus-5").unwrap();
        // 官方裸键（无 provider 前缀、无 @ 后缀）胜出，而不是 requesty 的欧区价
        assert_eq!(p, price(5e-6, 25e-6, 0.5e-6, 6.25e-6));
    }

    /// 全 0 占位价绝不能胜出：收下会把该模型整段成本抹成 0，
    /// 而查不到价至少还有三锚点均价兜底。
    #[test]
    fn placeholder_zero_price_never_wins_collision() {
        let mut m = HashMap::new();
        // 裸键形态最“干净”，但价格是占位 0；带前缀的才是真价
        m.insert("gpt-5-6-sol".to_string(), price(0.0, 0.0, 0.0, 0.0));
        m.insert("openai/gpt-5.6-sol".to_string(), price(5e-6, 30e-6, 0.5e-6, 6.25e-6));
        let t = PricingTable::new(m);
        assert_eq!(t.resolve("gpt-5.6-sol").unwrap(), price(5e-6, 30e-6, 0.5e-6, 6.25e-6));
    }

    /// 缓存单价缺失（退化成 0）的条目让位给齐全的条目：Claude 侧 cache_read
    /// 常占成本七成以上，取到缺缓存价的那条会把总额砍掉大半。
    #[test]
    fn entry_with_explicit_cache_prices_wins_collision() {
        let mut m = HashMap::new();
        m.insert("claude-opus-5".to_string(), price(5e-6, 25e-6, 0.0, 0.0));
        m.insert("vendor/claude-opus-5".to_string(), price(5e-6, 25e-6, 0.5e-6, 6.25e-6));
        let t = PricingTable::new(m);
        assert_eq!(t.resolve("claude-opus-5").unwrap().cache_read, 0.5e-6);
    }

    /// 锚点均价兜底同样不得抖：锚点自身也来自碰撞组。
    #[test]
    fn anchor_fallback_is_stable_across_rebuilds() {
        let build = || {
            let mut m = colliding_opus5_table();
            m.insert("claude-opus-4-8".to_string(), price(5e-6, 25e-6, 0.5e-6, 6.25e-6));
            m.insert("claude-opus-4-8@eu".to_string(), price(5.5e-6, 27.5e-6, 0.55e-6, 6.88e-6));
            PricingTable::new(m).resolve("codex-auto-review")
        };
        let expected = build().expect("锚点存在时必须有兜底均价");
        for round in 0..64 {
            assert_eq!(build().unwrap(), expected, "第 {round} 轮兜底均价漂移");
        }
    }

    #[test]
    fn cost_formula_includes_1h_surcharge_and_reasoning() {
        let p = ModelPrice {
            input: 1.0,
            output: 2.0,
            cache_read: 0.1,
            cache_write: 1.25,
        };
        let u = UsageTotals {
            input: 100,
            output: 10,
            reasoning: 5,
            cache_read: 1000,
            cache_write: 40,
            cache_write_1h: 20,
        };
        let expect = 100.0 * 1.0
            + (10.0 + 5.0) * 2.0
            + 40.0 * 1.25
            + 20.0 * 1.25 * 0.6
            + 1000.0 * 0.1;
        assert!((cost_of(&u, &p) - expect).abs() < 1e-9);
    }
}
