//! KPI 五格的数字滚动补间(`useTween.ts`)。
//!
//! 与 `mt_ui::motion::ValueTween`(排行条宽度用的那条)的区别:这里是 KPI 数字
//! 自己的一份 —— 400ms easeOutCubic、不过减弱动效那道闸、目标中途变了从**当前
//! 显示值**接着补。

use std::time::Instant;

use mt_usage::UsageStatsPayload;

use super::model::cache_hit_rate;

/// 补间时长(`useTween.ts:3`)。
const TWEEN_MS: f32 = 400.0;

/// easeOutCubic:`1 - (1-t)^3`,前快后缓。
fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// 一格 KPI 的数字补间。从**上一次显示值**补到新目标;
/// `from == to` 直接静止(不起动画)。
#[derive(Clone, Copy, Debug)]
pub(super) struct Tween {
    from: f64,
    to: f64,
    start: Instant,
}

impl Tween {
    fn still(v: f64) -> Self {
        Self {
            from: v,
            to: v,
            start: Instant::now(),
        }
    }

    /// 改目标:起点取**当前显示值**(动画中途目标又变时从当前值继续)。
    fn retarget(&mut self, to: f64, now: Instant) {
        let from = self.value(now);
        *self = Self {
            from,
            to,
            start: now,
        };
    }

    fn progress(&self, now: Instant) -> f32 {
        if self.from == self.to {
            return 1.0;
        }
        (now.duration_since(self.start).as_secs_f32() * 1000.0 / TWEEN_MS).clamp(0.0, 1.0)
    }

    pub(super) fn value(&self, now: Instant) -> f64 {
        let t = ease_out_cubic(self.progress(now));
        self.from + (self.to - self.from) * t as f64
    }

    fn done(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }
}

/// 五格 KPI 各自独立补间。
#[derive(Clone, Copy)]
pub(super) struct Tweens {
    pub(super) cost: Tween,
    pub(super) tokens: Tween,
    pub(super) calls: Tween,
    pub(super) sessions: Tween,
    pub(super) cache_hit: Tween,
}

impl Tweens {
    pub(super) fn zeroed() -> Self {
        Self {
            cost: Tween::still(0.0),
            tokens: Tween::still(0.0),
            calls: Tween::still(0.0),
            sessions: Tween::still(0.0),
            cache_hit: Tween::still(0.0),
        }
    }

    pub(super) fn retarget(&mut self, stats: &UsageStatsPayload, now: Instant) {
        // 总 token = 副行四项之和,与排行榜 tokens(后端 `UsageTotals::total`)同口径
        let tokens = stats.input_tokens
            + stats.output_tokens
            + stats.cache_read_tokens
            + stats.cache_write_tokens;
        let hit = cache_hit_rate(
            stats.input_tokens,
            stats.cache_read_tokens,
            stats.cache_write_tokens,
        )
        .unwrap_or(0.0);
        self.cost.retarget(stats.total_cost, now);
        self.tokens.retarget(tokens as f64, now);
        self.calls.retarget(stats.total_calls as f64, now);
        self.sessions.retarget(stats.session_count as f64, now);
        self.cache_hit.retarget(hit, now);
    }

    pub(super) fn running(&self, now: Instant) -> bool {
        [
            self.cost,
            self.tokens,
            self.calls,
            self.sessions,
            self.cache_hit,
        ]
        .iter()
        .any(|t| !t.done(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 数字滚动:400ms easeOutCubic,`from == to` 直接静止。
    #[test]
    fn 数字补间按_ease_out_cubic() {
        assert!((ease_out_cubic(0.0) - 0.0).abs() < 1e-6);
        assert!((ease_out_cubic(1.0) - 1.0).abs() < 1e-6);
        // 前快后缓:半程已经走过一半以上
        assert!(ease_out_cubic(0.5) > 0.8);

        let now = Instant::now();
        let still = Tween::still(42.0);
        assert!(still.done(now), "from == to 不该起动画");
        assert!((still.value(now) - 42.0).abs() < 1e-9);

        let mut t = Tween::still(0.0);
        t.retarget(100.0, now);
        assert!(!t.done(now));
        assert!((t.value(now) - 0.0).abs() < 1e-6, "起点是上一次显示值");
        let later = now + Duration::from_millis(400);
        assert!(t.done(later));
        assert!((t.value(later) - 100.0).abs() < 1e-6);
        // 超时之后不许越过目标
        assert!((t.value(now + Duration::from_secs(10)) - 100.0).abs() < 1e-6);
    }
}
