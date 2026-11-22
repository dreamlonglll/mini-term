//! 后台取数的**单飞**调度:同一时刻只跑一份,期间又有人要就记一个脏位、跑完当场补一趟;
//! 换仓库 / 换项目时作废在途的那份(请求令牌)。
//!
//! # 为什么不只用令牌
//!
//! 只有令牌(「后发作废先发」)时,先发的那趟照样在后台跑完 —— 大仓库一次 status 两三秒,
//! 工作区监听每 2s 交一批、`.git` 监听与 pty 嗅探也各叫一次,后台常驻一到两份全量 status
//! 叠着跑,谁先回来都不作数,白白抢 CPU。单飞之后:
//!
//! - 在途期间的请求一律并成一个脏位,跑完只补**一**趟(它开跑时读的是最新状态,前面攒的
//!   全都反映得出来);
//! - 在途那份照样落地(比画着的新),补跑那份再落一次 —— 不丢弃,否则变化一直不停时列表
//!   会永远停在旧样子。
//!
//! 「某一刻之后的变化有没有人去取」由 [`Flight::covers`] 回答:监听交来一批变化时先问一句,
//! 已经有一趟在那之后才开跑的取数(或排着补跑)就不必再要。
//!
//! 纯状态机,不碰 GPUI;发起 / 落地的胶水在各调用方(「更改」列表、amend 信息、文件树的
//! git 标记)。

use std::time::Instant;

/// 一趟取数落地时的判定(见 [`Flight::finish`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landing {
    /// 令牌对不上:发出之后作废过(换了仓库 / 项目)。结果丢掉,状态不动 —— 在跑的是新的那趟。
    Stale,
    /// 收下这份结果。`rerun` = 在途期间又有人要过,调用方落完这份当场再 [`Flight::begin`] 一趟。
    Done { rerun: bool },
}

/// 见模块注释。
#[derive(Debug, Default)]
pub struct Flight {
    token: u64,
    running: bool,
    dirty: bool,
    /// 最近一趟(在跑的或跑完的)开跑的时刻;作废后清空。
    started_at: Option<Instant>,
}

impl Flight {
    /// 要跑一趟。没在跑 → 返回这一趟的令牌,调用方真去跑;在跑 → 记脏位、返回 `None`
    /// (落地时 [`Landing::Done`] 带 `rerun`)。`now` 由调用方给(单测要能造先后)。
    pub fn begin(&mut self, now: Instant) -> Option<u64> {
        if self.running {
            self.dirty = true;
            return None;
        }
        self.token = self.token.wrapping_add(1);
        self.running = true;
        self.started_at = Some(now);
        Some(self.token)
    }

    /// 令牌为 `token` 的那趟跑完了。
    pub fn finish(&mut self, token: u64) -> Landing {
        if !self.running || token != self.token {
            return Landing::Stale;
        }
        self.running = false;
        Landing::Done {
            rerun: std::mem::take(&mut self.dirty),
        }
    }

    /// 作废在途的那趟(换仓库 / 换项目):它的结果回来一律 [`Landing::Stale`],脏位清掉,
    /// 下一次 [`Self::begin`] 立刻能跑。
    pub fn reset(&mut self) {
        self.token = self.token.wrapping_add(1);
        self.running = false;
        self.dirty = false;
        self.started_at = None;
    }

    #[cfg(test)]
    fn is_running(&self) -> bool {
        self.running
    }

    /// `at` 那一刻之前的变化,有没有一趟取数一定会反映出来:最近一趟在它之后才开跑(在跑的
    /// 回来就落地,跑完的已经落地),或者在途期间有人要过、补跑排着(补跑开跑只会更晚)。
    pub fn covers(&self, at: Instant) -> bool {
        (self.running && self.dirty) || self.started_at.is_some_and(|started| started >= at)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn ms(base: Instant, n: u64) -> Instant {
        base + Duration::from_millis(n)
    }

    /// 在途期间的请求并成一趟补跑;补跑那趟落地后不再补。
    #[test]
    fn 在途合并成一趟补跑() {
        let t0 = Instant::now();
        let mut f = Flight::default();
        let first = f.begin(t0).expect("空闲时能跑");
        assert!(f.is_running());
        assert_eq!(f.begin(ms(t0, 10)), None, "在途:只记脏");
        assert_eq!(f.begin(ms(t0, 20)), None, "再要也只是同一个脏位");
        assert_eq!(f.finish(first), Landing::Done { rerun: true });
        assert!(!f.is_running());
        let second = f.begin(ms(t0, 30)).expect("补跑");
        assert_ne!(second, first);
        assert_eq!(f.finish(second), Landing::Done { rerun: false });
    }

    /// 作废之后在途那趟回来是过期的,且不影响新开的那趟。
    #[test]
    fn 作废后迟到结果丢弃() {
        let t0 = Instant::now();
        let mut f = Flight::default();
        let old = f.begin(t0).unwrap();
        assert_eq!(f.begin(ms(t0, 1)), None);
        f.reset();
        assert!(!f.is_running());
        let new = f.begin(ms(t0, 2)).expect("作废后立刻能跑");
        assert_eq!(f.finish(old), Landing::Stale, "旧令牌");
        assert!(f.is_running(), "过期结果不许把新的那趟标成跑完");
        assert_eq!(
            f.finish(new),
            Landing::Done { rerun: false },
            "作废时脏位一并清掉"
        );
        assert_eq!(f.finish(new), Landing::Stale, "同一趟只落一次");
    }

    /// 「某一刻之前的变化有没有人取」:开跑晚于它才算;在途且有补跑排着也算。
    #[test]
    fn 覆盖判定按开跑时刻() {
        let t0 = Instant::now();
        let mut f = Flight::default();
        assert!(!f.covers(t0), "从没跑过");
        let token = f.begin(ms(t0, 100)).unwrap();
        assert!(f.covers(ms(t0, 100)), "同一刻开跑:读盘不早于它");
        assert!(f.covers(ms(t0, 50)));
        assert!(!f.covers(ms(t0, 150)), "开跑之后的变化它不一定读得到");
        assert_eq!(f.begin(ms(t0, 160)), None);
        assert!(f.covers(ms(t0, 150)), "补跑排着,开跑只会更晚");
        assert_eq!(f.finish(token), Landing::Done { rerun: true });
        assert!(!f.covers(ms(t0, 150)), "补跑还没开跑:调用方会当场 begin");
        f.begin(ms(t0, 170)).unwrap();
        assert!(f.covers(ms(t0, 150)));
        f.reset();
        assert!(!f.covers(t0), "作废后不再算数");
    }
}
