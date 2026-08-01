use serde::Deserialize;
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

impl PricingTable {
    pub fn new(raw: HashMap<String, ModelPrice>) -> Self {
        let mut exact = HashMap::with_capacity(raw.len());
        for (k, v) in raw {
            exact.insert(canonical(&k), v);
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
            if boundary_ok && best.is_none_or(|(bk, _)| k.len() > bk.len()) {
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
