//! 行级 damage 追踪：只重建「内容真的变了」的行。
//!
//! # 为什么不用 alacritty 的 `Term::damage()`
//!
//! alacritty 自带一套 damage 记账（[`alacritty_terminal::term::TermDamage`]），但它
//! 有两条对我们致命的性质：
//!
//! 1. **滚动一律标记全量失效**。`damage()` 的注释写得很直白：display_offset 非零
//!    时内容整体位移，走 full damage。而「大输出滚屏」恰恰是我们最想省的场景 ——
//!    一屏 50 行往上滚 3 行，49 行的**内容一个字没变**，只是换了个 y。
//! 2. **它是单消费者状态机**：`damage()` 要 `&mut Term`，读完必须 `reset_damage()`，
//!    并且顺手改写内部的 `last_cursor`。渲染层一旦开始消费，任何别的调用方
//!    （诊断、镜像、测试）再调一次就会拿到空 damage —— 这是很难查的耦合。
//!
//! 所以这里走**行内容签名**：把一行里所有影响绘制的东西（字符 + 组合符号 + 解析后
//! 的前后景色 + 属性位 + 是否被选中 + 是否光标格）哈希成一个 u64，用签名而不是行号
//! 做缓存键。
//!
//! # 用签名而不是行号做键，换来什么
//!
//! 滚屏时一行从第 20 行挪到第 17 行，**签名不变**：缓存直接命中，只是放置到新的 y。
//! 于是「滚 3 行」的代价就真的只有 3 行的 shape，而不是 50 行。行内相对坐标是这条
//! 的前提 —— 缓存里存的几何全部相对该行左上角，摆到哪一行是放置时才决定的。
//!
//! 副作用还有一条：同一帧里内容相同的多行（大片空行、重复的表格线）共用一份缓存。
//!
//! # 哈希碰撞
//!
//! 64 位 SipHash-1-3（std 默认）。碰撞意味着「把另一行的画面贴到这一行」，概率在
//! 每帧几十行、缓存几百条的量级下可以忽略（生日界约 2^-45 量级/帧）。这是所有
//! 基于内容哈希的增量渲染共有的取舍，记在这里以免将来有人当灵异事件查。
//!
//! # 什么会让整表作废
//!
//! 签名只覆盖**格子内容**。cell 尺寸、字体、主题、列数、聚焦态这些「全局参数」变了，
//! 每一行的画面都会变但签名不动 —— 它们进 [`FrameKey`]，变了就清空整表。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use alacritty_terminal::term::cell::Flags;
use gpui::Hsla;

/// 一个格子最多带几个组合符号。与 alacritty 内部上限一致。
pub const MAX_ZEROWIDTH_CHARS: usize = 5;

/// 参与行签名的、**一个格子的全部可见属性**。
///
/// 颜色取的是**解析之后**的值：OSC 4 改调色板、主题切换都会让它变，
/// 不需要额外的失效通道。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellSignature {
    /// 列号。同样的字符挪了列就是不同的画面。
    pub col: usize,
    pub ch: char,
    /// 组合符号（不足补 `\0`）。
    pub zerowidth: [char; MAX_ZEROWIDTH_CHARS],
    pub fg: Hsla,
    pub bg: Hsla,
    /// 背景是不是「默认背景」（决定发不发 quad，背景图能否透出）。
    pub bg_default: bool,
    pub flags: Flags,
    pub selected: bool,
    /// 0 = 不是光标格；否则是光标形状的判别值 + 1。
    pub cursor: u8,
}

impl CellSignature {
    /// 造一个只有字符的最简格子（测试与占位用）。
    pub fn plain(col: usize, ch: char, fg: Hsla, bg: Hsla) -> Self {
        Self {
            col,
            ch,
            zerowidth: ['\0'; MAX_ZEROWIDTH_CHARS],
            fg,
            bg,
            bg_default: true,
            flags: Flags::empty(),
            selected: false,
            cursor: 0,
        }
    }
}

fn hash_hsla<H: Hasher>(color: &Hsla, state: &mut H) {
    // f32 没有 Hash（NaN 语义），按位哈希。终端配色里不会出现 NaN，
    // 真出现了也只是「两个 NaN 被当成不同的颜色」，多重建一行而已。
    color.h.to_bits().hash(state);
    color.s.to_bits().hash(state);
    color.l.to_bits().hash(state);
    color.a.to_bits().hash(state);
}

impl Hash for CellSignature {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.col.hash(state);
        self.ch.hash(state);
        self.zerowidth.hash(state);
        hash_hsla(&self.fg, state);
        hash_hsla(&self.bg, state);
        self.bg_default.hash(state);
        self.flags.bits().hash(state);
        self.selected.hash(state);
        self.cursor.hash(state);
    }
}

/// 一行的内容签名。空行（全部是默认属性的空格）也会得到一个稳定值。
pub fn row_signature(cells: &[CellSignature]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    // 长度先进哈希：`["ab"]` 与 `["a","b"]` 这类拼接歧义直接堵死。
    cells.len().hash(&mut hasher);
    for cell in cells {
        cell.hash(&mut hasher);
    }
    hasher.finish()
}

/// 「全局渲染参数」的指纹。任一分量变化 ⇒ 整张缓存作废。
///
/// 刻意做成一个不透明的 u64：调用方用 [`FrameKey::builder`] 往里塞任何
/// 实现了 `Hash` 的东西，新增一个参数不需要改这里的结构。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameKey(u64);

impl FrameKey {
    pub fn builder() -> FrameKeyBuilder {
        FrameKeyBuilder(std::collections::hash_map::DefaultHasher::new())
    }

    pub fn raw(&self) -> u64 {
        self.0
    }
}

pub struct FrameKeyBuilder(std::collections::hash_map::DefaultHasher);

impl FrameKeyBuilder {
    pub fn push(mut self, value: impl Hash) -> Self {
        value.hash(&mut self.0);
        self
    }

    /// 浮点参数（cell 宽高、字号）按位进哈希。
    pub fn push_f32(self, value: f32) -> Self {
        self.push(value.to_bits())
    }

    pub fn push_hsla(mut self, color: Hsla) -> Self {
        hash_hsla(&color, &mut self.0);
        self
    }

    pub fn finish(self) -> FrameKey {
        FrameKey(self.0.finish())
    }
}

/// 累计的失效统计。给「量化对比」用，也是回归测试的断言口。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DamageStats {
    /// 处理过的帧数。
    pub frames: u64,
    /// 累计遇到的行数（= Σ 每帧可视行数）。
    pub rows_seen: u64,
    /// 累计**真正重建**（重新 shape）的行数。
    pub rows_rebuilt: u64,
    /// 整表作废的次数（换主题 / 改字号 / 窗口尺寸变化）。
    pub full_invalidations: u64,
}

impl DamageStats {
    /// 省下来的行占比，0.0 ~ 1.0。没有样本时返回 0。
    pub fn saved_ratio(&self) -> f64 {
        if self.rows_seen == 0 {
            return 0.0;
        }
        1.0 - (self.rows_rebuilt as f64 / self.rows_seen as f64)
    }

    /// 平均每帧重建行数。
    pub fn rows_rebuilt_per_frame(&self) -> f64 {
        if self.frames == 0 {
            return 0.0;
        }
        self.rows_rebuilt as f64 / self.frames as f64
    }
}

struct CacheEntry<T> {
    value: T,
    last_used: u64,
}

/// 「行签名 → 渲染产物」的缓存。
///
/// `T` 一般是 `Rc<RowRender>`（clone 只是加引用计数）。做成泛型纯粹是为了
/// 单测能在没有 gpui `Window` 的情况下把失效逻辑跑完。
pub struct RowCache<T> {
    key: FrameKey,
    entries: HashMap<u64, CacheEntry<T>>,
    frame: u64,
    stats: DamageStats,
    /// 本帧已见行数，`end_frame` 时并进 stats。
    rows_this_frame: u64,
    rebuilt_this_frame: u64,
}

impl<T: Clone> Default for RowCache<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone> RowCache<T> {
    pub fn new() -> Self {
        Self {
            key: FrameKey::default(),
            entries: HashMap::new(),
            frame: 0,
            stats: DamageStats::default(),
            rows_this_frame: 0,
            rebuilt_this_frame: 0,
        }
    }

    /// 开一帧。`key` 与上一帧不同就清空整表，返回 `true`。
    ///
    /// 首帧（`frame == 0`）不算「整表作废」：那时候本来就是空表。
    pub fn begin_frame(&mut self, key: FrameKey) -> bool {
        self.frame += 1;
        self.rows_this_frame = 0;
        self.rebuilt_this_frame = 0;
        if self.key == key {
            return false;
        }
        let was_populated = !self.entries.is_empty();
        self.entries.clear();
        self.key = key;
        if was_populated {
            self.stats.full_invalidations += 1;
        }
        was_populated
    }

    /// 查一行。命中返回缓存值（并续命），未命中返回 `None` —— 调用方重建后
    /// 必须调 [`Self::insert`]，否则统计会失真。
    pub fn get(&mut self, signature: u64) -> Option<T> {
        self.rows_this_frame += 1;
        let frame = self.frame;
        match self.entries.get_mut(&signature) {
            Some(entry) => {
                entry.last_used = frame;
                Some(entry.value.clone())
            }
            None => {
                self.rebuilt_this_frame += 1;
                None
            }
        }
    }

    pub fn insert(&mut self, signature: u64, value: T) {
        let frame = self.frame;
        self.entries.insert(
            signature,
            CacheEntry {
                value,
                last_used: frame,
            },
        );
    }

    /// 收一帧：结算统计 + 淘汰。
    ///
    /// `live_rows` 是本帧的可视行数，缓存容量按它的倍数封顶 —— 留几屏的余量让
    /// 「滚下去又滚回来」也能命中，但不能无限涨（一个刷屏进程能造出百万种行）。
    pub fn end_frame(&mut self, live_rows: usize) {
        self.stats.frames += 1;
        self.stats.rows_seen += self.rows_this_frame;
        self.stats.rows_rebuilt += self.rebuilt_this_frame;

        let cap = (live_rows * 4).max(256);
        if self.entries.len() > cap {
            // 超限就只留本帧用过的：一次收敛到「刚好一屏」，不做 LRU 排序
            // （排序要 O(n log n)，而这条路本身就是异常路径）。
            let frame = self.frame;
            self.entries.retain(|_, e| e.last_used == frame);
        }
    }

    pub fn stats(&self) -> DamageStats {
        self.stats
    }

    /// 缓存条目数（诊断用）。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsla(l: f32) -> Hsla {
        Hsla {
            h: 0.0,
            s: 0.0,
            l,
            a: 1.0,
        }
    }

    /// 造一行 `cols` 列的签名格子，内容取自 `text`（不足补空格）。
    fn row(text: &str, cols: usize) -> Vec<CellSignature> {
        let fg = hsla(0.9);
        let bg = hsla(0.1);
        let mut chars: Vec<char> = text.chars().collect();
        chars.resize(cols, ' ');
        chars
            .into_iter()
            .enumerate()
            .map(|(col, ch)| CellSignature::plain(col, ch, fg, bg))
            .collect()
    }

    #[test]
    fn 相同内容签名相同_不同内容签名不同() {
        assert_eq!(row_signature(&row("hello", 20)), row_signature(&row("hello", 20)));
        assert_ne!(row_signature(&row("hello", 20)), row_signature(&row("hellp", 20)));
        // 同样的字挪了列也是不同的画面
        let mut shifted = row("hello", 20);
        shifted[0].col = 9;
        assert_ne!(row_signature(&row("hello", 20)), row_signature(&shifted));
    }

    #[test]
    fn 选中态与光标态进签名() {
        let base = row("abc", 10);
        let mut selected = base.clone();
        selected[1].selected = true;
        assert_ne!(row_signature(&base), row_signature(&selected));

        let mut cursor = base.clone();
        cursor[1].cursor = 1;
        assert_ne!(row_signature(&base), row_signature(&cursor));
        // 光标形状换了也要变（块 → 竖线）
        let mut beam = base.clone();
        beam[1].cursor = 3;
        assert_ne!(row_signature(&cursor), row_signature(&beam));
    }

    #[test]
    fn 颜色与属性位进签名() {
        let base = row("abc", 10);
        let mut recolored = base.clone();
        recolored[0].fg = hsla(0.5);
        assert_ne!(row_signature(&base), row_signature(&recolored));

        let mut bold = base.clone();
        bold[0].flags = Flags::BOLD;
        assert_ne!(row_signature(&base), row_signature(&bold));

        // 「默认背景」的语义位单独进签名：解析后的 RGB 可能与某个 ANSI 色撞色，
        // 但一个发 quad 一个不发，画面并不一样
        let mut opaque = base.clone();
        opaque[0].bg_default = false;
        assert_ne!(row_signature(&base), row_signature(&opaque));
    }

    #[test]
    fn 组合符号进签名() {
        let base = row("e", 4);
        let mut combining = base.clone();
        combining[0].zerowidth[0] = '\u{0301}'; // 锐音符
        assert_ne!(row_signature(&base), row_signature(&combining));
    }

    /// 场景 A：静止画面里打字 —— 每帧只有光标行 + 提示行变化。
    #[test]
    fn 场景_打字时每帧只重建两行() {
        const ROWS: usize = 50;
        let mut cache: RowCache<u64> = RowCache::new();
        let key = FrameKey::builder().push("14px").finish();

        // 首帧：全部是新的
        let mut sigs: Vec<u64> = (0..ROWS)
            .map(|r| row_signature(&row(&format!("line {r}"), 80)))
            .collect();
        run_frame(&mut cache, key, &sigs);
        assert_eq!(cache.stats().rows_rebuilt, ROWS as u64);

        // 之后 20 帧：只有最后两行（提示行 + 光标行）在变
        for i in 0..20 {
            sigs[ROWS - 1] = row_signature(&row(&format!("$ echo {i}"), 80));
            sigs[ROWS - 2] = row_signature(&row(&format!("out {i}"), 80));
            run_frame(&mut cache, key, &sigs);
        }

        let stats = cache.stats();
        assert_eq!(stats.frames, 21);
        assert_eq!(stats.rows_seen, 21 * ROWS as u64);
        // 首帧 50 + 20 帧 × 2 = 90
        assert_eq!(stats.rows_rebuilt, 50 + 20 * 2);
        assert!(stats.saved_ratio() > 0.91, "实际 {}", stats.saved_ratio());
    }

    /// 场景 B：大输出滚屏 —— 每帧往上滚 3 行。
    ///
    /// 这正是 alacritty 自带 damage 会标全量失效、而内容签名能救回来的场景：
    /// 49 行只是换了 y，签名一个字没变。
    #[test]
    fn 场景_滚屏每帧只重建新进来的行() {
        const ROWS: usize = 50;
        const SCROLL: usize = 3;
        const FRAMES: usize = 30;
        let mut cache: RowCache<u64> = RowCache::new();
        let key = FrameKey::builder().push("14px").finish();

        let mut next_line = 0usize;
        let mut viewport: Vec<u64> = (0..ROWS)
            .map(|_| {
                let s = row_signature(&row(&format!("output line {next_line}"), 80));
                next_line += 1;
                s
            })
            .collect();
        run_frame(&mut cache, key, &viewport);

        for _ in 0..FRAMES {
            viewport.drain(0..SCROLL);
            for _ in 0..SCROLL {
                viewport.push(row_signature(&row(&format!("output line {next_line}"), 80)));
                next_line += 1;
            }
            run_frame(&mut cache, key, &viewport);
        }

        let stats = cache.stats();
        assert_eq!(stats.frames, (FRAMES + 1) as u64);
        // 首帧 50 行 + 每帧新进 3 行
        assert_eq!(stats.rows_rebuilt, (ROWS + FRAMES * SCROLL) as u64);
        assert!(
            stats.rows_rebuilt_per_frame() < 5.0,
            "每帧重建 {} 行",
            stats.rows_rebuilt_per_frame()
        );
    }

    /// 场景 C：换主题 / 改字号 —— FrameKey 变，整表作废。
    #[test]
    fn 帧指纹变化触发全量失效() {
        const ROWS: usize = 20;
        let mut cache: RowCache<u64> = RowCache::new();
        let sigs: Vec<u64> = (0..ROWS)
            .map(|r| row_signature(&row(&format!("line {r}"), 40)))
            .collect();

        let key_a = FrameKey::builder().push_f32(14.0).push_hsla(hsla(0.1)).finish();
        run_frame(&mut cache, key_a, &sigs);
        run_frame(&mut cache, key_a, &sigs);
        assert_eq!(cache.stats().rows_rebuilt, ROWS as u64, "第二帧应全命中");

        // 只改了背景色 → 每一行的画面都变了，但签名不动，必须靠 FrameKey 兜住
        let key_b = FrameKey::builder().push_f32(14.0).push_hsla(hsla(0.9)).finish();
        run_frame(&mut cache, key_b, &sigs);
        let stats = cache.stats();
        assert_eq!(stats.rows_rebuilt, 2 * ROWS as u64);
        assert_eq!(stats.full_invalidations, 1);
    }

    /// 同一帧里内容相同的多行共用一份缓存（大片空行 / 重复表格线）。
    #[test]
    fn 同帧重复行只重建一次() {
        let mut cache: RowCache<u64> = RowCache::new();
        let key = FrameKey::builder().push(1u8).finish();
        let blank = row_signature(&row("", 80));
        let sigs = vec![blank; 40];
        run_frame(&mut cache, key, &sigs);
        assert_eq!(cache.stats().rows_rebuilt, 1);
        assert_eq!(cache.stats().rows_seen, 40);
    }

    /// 刷屏进程能造出无穷多种行，缓存必须封顶。
    #[test]
    fn 缓存容量封顶() {
        let mut cache: RowCache<u64> = RowCache::new();
        let key = FrameKey::builder().push(1u8).finish();
        let mut n = 0u64;
        for _ in 0..200 {
            let sigs: Vec<u64> = (0..50)
                .map(|_| {
                    n += 1;
                    row_signature(&row(&format!("unique {n}"), 80))
                })
                .collect();
            run_frame(&mut cache, key, &sigs);
        }
        // cap = max(256, 50*4) = 256；超限那一帧收敛回「本帧用过的」= 50
        assert!(cache.len() <= 256, "缓存涨到 {}", cache.len());
    }

    fn run_frame(cache: &mut RowCache<u64>, key: FrameKey, sigs: &[u64]) {
        cache.begin_frame(key);
        for sig in sigs {
            if cache.get(*sig).is_none() {
                cache.insert(*sig, *sig);
            }
        }
        cache.end_frame(sigs.len());
    }
}
