//! 终端滚动条(改造清单 #11)。
//!
//! # 原版是什么
//!
//! xterm.js 把回看缓冲放在一个可滚动的 `.xterm-viewport` 里,滚动条是**浏览器原生的**,
//! 靠 `src/styles.css` 的 `::-webkit-scrollbar` 规则化妆:宽 6px、轨道透明、
//! 滑块 `--border-default`(白 8%)、圆角 3px、hover 转 `--border-strong`。
//!
//! GPUI 侧没有「可滚动容器」这层免费午餐 —— grid 是我们自己按 `display_offset` 画的,
//! 所以滚动条也得自己画。视觉照抄上面四条;多出来的是**闲置淡出**(原生滚动条在
//! WebView2 里是常显的,一直挂一条竖线在终端右边其实挺吵)。
//!
//! # 三条硬规则
//!
//! 1. **alt screen 不画**。vim / less / htop 自己管翻页,没有回看缓冲,
//!    画一条永远满格的滚动条是纯误导;
//! 2. **不打穿 damage 缓存**。滚动条只在 `paint` 阶段发 quad,既不进
//!    [`super::damage::RowCache`] 的行签名,也不进帧指纹 —— 拖滑块时
//!    行缓存该命中还是命中(滚屏行只是换个 y);
//! 3. **命中优先于选择**。滑块区域内按下左键必须只拖滚动条,不能顺带起一个选区。
//!    判据由 [`ScrollbarHit`] 给,`element.rs` 的按下/移动处理器先问它。
//!
//! # 几何
//!
//! ```text
//! total = history + screen        ← 整条内容有多长
//! top   = total - screen - offset ← 视口顶在第几行(offset = display_offset)
//!
//! thumb_h = max(track * screen / total, min_thumb)
//! thumb_y = (track - thumb_h) * top / (total - screen)
//! ```
//!
//! 注意 `thumb_y` 的分母是 `total - screen`(可滚动的行数)而不是 `total` ——
//! 用 `total` 会让滑到底时滑块底边差一截够不到轨道底,看着像「还能再滚」。

use std::time::Duration;

use gpui::{Bounds, Hsla, Pixels, Point, Size, point, px, size};

/// 滚动条外观与行为。默认值照抄 `styles.css` 的 `::-webkit-scrollbar`。
#[derive(Clone, Debug, PartialEq)]
pub struct ScrollbarStyle {
    /// 关掉滚动条(宿主想完全按旧行为走时)。
    pub enabled: bool,
    /// 轨道宽度。原版 `width: 6px`。
    pub width: Pixels,
    /// 滑块最小高度。内容极长时不至于缩成一个点点不到。
    pub min_thumb: Pixels,
    /// 滑块圆角。原版 `border-radius: 3px`。
    pub radius: Pixels,
    /// 右侧留白(滑块不贴死边)。
    pub inset: Pixels,
    /// 滑块颜色。`None` = 取 `TerminalTheme::foreground` 压到 `idle_alpha`。
    pub thumb: Option<Hsla>,
    /// hover / 拖动时的滑块颜色。`None` = 同上但用 `active_alpha`。
    pub thumb_active: Option<Hsla>,
    /// 轨道颜色。`None` = 不画轨道(原版 `background: transparent`)。
    pub track: Option<Hsla>,
    /// 自动取色时的静息透明度(原版 `--border-default` 是白 8%)。
    pub idle_alpha: f32,
    /// 自动取色时的活跃透明度(原版 hover 转 `--border-strong`)。
    pub active_alpha: f32,
    /// 停止操作后多久开始淡出。
    pub fade_delay: Duration,
    /// 淡出持续多久。设成 0 = 不淡出(立刻落到静息透明度)。
    pub fade_duration: Duration,
    /// 淡出后的残留强度(0..1 的系数)。**不在底部**时保留一点,
    /// 让「我还在回看」这件事一直看得见;回到底部则彻底消失。
    pub resting: f32,
}

impl Default for ScrollbarStyle {
    fn default() -> Self {
        Self {
            enabled: true,
            width: px(6.0),
            min_thumb: px(24.0),
            radius: px(3.0),
            inset: px(1.0),
            thumb: None,
            thumb_active: None,
            track: None,
            // 原版 --border-default = rgba(255,255,255,.08) 压在深色底上;
            // 这里按前景色取,浅色主题下同样是「低调的一条」
            idle_alpha: 0.16,
            active_alpha: 0.34,
            fade_delay: Duration::from_millis(900),
            fade_duration: Duration::from_millis(450),
            resting: 0.5,
        }
    }
}

/// 一帧算出来的滚动条几何。`None` 表示这一帧不该画(没有回看缓冲 / alt screen)。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarLayout {
    /// 轨道矩形(元素坐标)。
    pub track: Bounds<Pixels>,
    /// 滑块矩形(元素坐标)。
    pub thumb: Bounds<Pixels>,
    /// 可滚动的总行数(`total - screen`),拖动换算要用。
    pub scrollable_lines: usize,
    /// 当前 `display_offset`。
    pub display_offset: usize,
    /// 一屏多少行(点轨道翻页要用)。
    pub screen_lines: usize,
}

/// 鼠标落在滚动条的哪儿。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollbarHit {
    /// 不在滚动条上,交给终端自己处理(选择 / 上报)。
    Miss,
    /// 在滑块上:按下即开始拖动。
    Thumb,
    /// 在轨道空白处:按下即翻一页。
    Track,
}

/// 滑块与轨道的几何计算。**纯函数,单测钉在这上面**。
///
/// - `total_lines` = history + screen;
/// - `display_offset` = 从底部往上滚了多少行(0 = 底部);
/// - 回看缓冲为空(`total <= screen`)返回 `None`。
pub fn layout(
    bounds: Bounds<Pixels>,
    style: &ScrollbarStyle,
    total_lines: usize,
    screen_lines: usize,
    display_offset: usize,
) -> Option<ScrollbarLayout> {
    if !style.enabled || screen_lines == 0 || total_lines <= screen_lines {
        return None;
    }
    let scrollable = total_lines - screen_lines;
    let offset = display_offset.min(scrollable);

    let track_h = f32::from(bounds.size.height);
    if track_h <= 0.0 {
        return None;
    }
    let track_x = f32::from(bounds.origin.x) + f32::from(bounds.size.width)
        - f32::from(style.width)
        - f32::from(style.inset);
    let track = Bounds::new(
        point(px(track_x), bounds.origin.y),
        size(style.width, bounds.size.height),
    );

    let ratio = screen_lines as f32 / total_lines as f32;
    let thumb_h = (track_h * ratio).max(f32::from(style.min_thumb)).min(track_h);
    // 顶在第几行:offset 越大越靠上
    let top_line = (scrollable - offset) as f32;
    let thumb_y = if scrollable == 0 {
        0.0
    } else {
        (track_h - thumb_h) * (top_line / scrollable as f32)
    };
    let thumb = Bounds::new(
        point(px(track_x), px(f32::from(bounds.origin.y) + thumb_y)),
        size(style.width, px(thumb_h)),
    );

    Some(ScrollbarLayout {
        track,
        thumb,
        scrollable_lines: scrollable,
        display_offset: offset,
        screen_lines,
    })
}

impl ScrollbarLayout {
    /// 鼠标位置落在哪。轨道左右各放宽一点点,6px 的条子精确命中太难点。
    pub fn hit(&self, pos: Point<Pixels>, grab_slack: Pixels) -> ScrollbarHit {
        let slack = f32::from(grab_slack);
        let x = f32::from(pos.x);
        let left = f32::from(self.track.origin.x) - slack;
        let right = f32::from(self.track.origin.x) + f32::from(self.track.size.width) + slack;
        if x < left || x > right {
            return ScrollbarHit::Miss;
        }
        let y = f32::from(pos.y);
        let top = f32::from(self.track.origin.y);
        let bottom = top + f32::from(self.track.size.height);
        if y < top || y > bottom {
            return ScrollbarHit::Miss;
        }
        let ty = f32::from(self.thumb.origin.y);
        let tb = ty + f32::from(self.thumb.size.height);
        if y >= ty && y <= tb {
            ScrollbarHit::Thumb
        } else {
            ScrollbarHit::Track
        }
    }

    /// 把「滑块顶边应该落在轨道的第几像素」换算成 `display_offset`。
    ///
    /// 拖动时调:`thumb_top = 鼠标 y - 按下时鼠标相对滑块顶边的偏移`。
    pub fn offset_for_thumb_top(&self, thumb_top: Pixels) -> usize {
        let track_h = f32::from(self.track.size.height);
        let thumb_h = f32::from(self.thumb.size.height);
        let span = track_h - thumb_h;
        if span <= 0.0 || self.scrollable_lines == 0 {
            return 0;
        }
        let rel = (f32::from(thumb_top) - f32::from(self.track.origin.y)).clamp(0.0, span);
        let top_line = (rel / span) * self.scrollable_lines as f32;
        // offset 是「从底部往上」,与 top_line 反向
        let offset = self.scrollable_lines as f32 - top_line;
        offset.round().clamp(0.0, self.scrollable_lines as f32) as usize
    }

    /// 点轨道空白处:往鼠标那一侧翻一页,返回新的 `display_offset`。
    pub fn offset_for_track_click(&self, pos: Point<Pixels>) -> usize {
        let page = self.screen_lines.max(1);
        let above = f32::from(pos.y) < f32::from(self.thumb.origin.y);
        if above {
            (self.display_offset + page).min(self.scrollable_lines)
        } else {
            self.display_offset.saturating_sub(page)
        }
    }

    /// 已经滚到底了吗(淡出规则要用)。
    pub fn at_bottom(&self) -> bool {
        self.display_offset == 0
    }
}

/// 滚动条这一帧该有多不透明。**纯函数**。
///
/// - `active` = 正在拖 / 鼠标悬在条上 → 恒定全不透明;
/// - 闲置未超过 `fade_delay` → 全不透明;
/// - 之后在 `fade_duration` 内线性衰减到 `resting`(不在底部)或 0(在底部)。
pub fn alpha(style: &ScrollbarStyle, idle: Duration, active: bool, at_bottom: bool) -> f32 {
    if active {
        return 1.0;
    }
    let floor = if at_bottom { 0.0 } else { style.resting };
    if idle < style.fade_delay {
        return 1.0;
    }
    if style.fade_duration.is_zero() {
        return floor;
    }
    let t = (idle - style.fade_delay).as_secs_f32() / style.fade_duration.as_secs_f32();
    if t >= 1.0 {
        floor
    } else {
        floor + (1.0 - floor) * (1.0 - t)
    }
}

/// 淡出还没走完 → 还要请求下一帧。为 0 时可以停下,不再空转。
pub fn needs_animation_frame(style: &ScrollbarStyle, idle: Duration, active: bool) -> bool {
    if active {
        return false;
    }
    idle < style.fade_delay + style.fade_duration
}

/// 滚动条的跨帧交互状态。挂在 `TerminalElementState` 上。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollbarDrag {
    /// 正在拖动;记录按下时鼠标相对滑块顶边的偏移(px)。
    pub grab_offset: Option<f32>,
    /// 鼠标是否悬在滚动条上。
    pub hovered: bool,
}

impl ScrollbarDrag {
    pub fn active(&self) -> bool {
        self.grab_offset.is_some() || self.hovered
    }
}

/// 尺寸的一个小工具:把 `Bounds` 往内缩一圈(画圆角滑块时不必要,留给宿主)。
pub fn shrink(bounds: Bounds<Pixels>, by: Pixels) -> Bounds<Pixels> {
    let b = f32::from(by);
    Bounds::new(
        point(px(f32::from(bounds.origin.x) + b), px(f32::from(bounds.origin.y) + b)),
        Size {
            width: px((f32::from(bounds.size.width) - 2.0 * b).max(0.0)),
            height: px((f32::from(bounds.size.height) - 2.0 * b).max(0.0)),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(0.0), px(0.0)), size(px(w), px(h)))
    }

    #[test]
    fn 没有回看缓冲就不画() {
        let s = ScrollbarStyle::default();
        assert!(layout(bounds(400.0, 300.0), &s, 30, 30, 0).is_none());
        assert!(layout(bounds(400.0, 300.0), &s, 10, 30, 0).is_none());
        // 关掉也不画
        let off = ScrollbarStyle {
            enabled: false,
            ..s
        };
        assert!(layout(bounds(400.0, 300.0), &off, 1000, 30, 0).is_none());
    }

    #[test]
    fn 滑块高度按视口占比_有下限() {
        let s = ScrollbarStyle::default();
        // 30/100 屏 → 300px 轨道上 90px
        let l = layout(bounds(400.0, 300.0), &s, 100, 30, 0).unwrap();
        assert!((f32::from(l.thumb.size.height) - 90.0).abs() < 0.01);
        // 内容极长时不能缩成一个点
        let l = layout(bounds(400.0, 300.0), &s, 100_000, 30, 0).unwrap();
        assert_eq!(l.thumb.size.height, s.min_thumb);
    }

    #[test]
    fn 滑到底与滚到顶都贴齐轨道端点() {
        let s = ScrollbarStyle::default();
        let track_h = 300.0;
        // 底部:offset = 0
        let l = layout(bounds(400.0, track_h), &s, 100, 30, 0).unwrap();
        let bottom = f32::from(l.thumb.origin.y) + f32::from(l.thumb.size.height);
        assert!(
            (bottom - track_h).abs() < 0.01,
            "滑到底时滑块底边必须贴轨道底,实际 {bottom}"
        );
        // 顶部:offset = scrollable
        let l = layout(bounds(400.0, track_h), &s, 100, 30, 70).unwrap();
        assert!(f32::from(l.thumb.origin.y).abs() < 0.01, "滚到顶要贴轨道顶");
        // 越界的 offset 会被钳住
        let l = layout(bounds(400.0, track_h), &s, 100, 30, 999).unwrap();
        assert_eq!(l.display_offset, 70);
    }

    #[test]
    fn 拖动换算与几何互为逆运算() {
        let s = ScrollbarStyle::default();
        for offset in [0usize, 1, 17, 35, 69, 70] {
            let l = layout(bounds(400.0, 300.0), &s, 100, 30, offset).unwrap();
            let back = l.offset_for_thumb_top(l.thumb.origin.y);
            assert_eq!(back, offset, "offset={offset} 往返对不上");
        }
    }

    #[test]
    fn 拖出轨道之外被钳住() {
        let s = ScrollbarStyle::default();
        let l = layout(bounds(400.0, 300.0), &s, 100, 30, 35).unwrap();
        assert_eq!(l.offset_for_thumb_top(px(-500.0)), 70, "拖过头 = 滚到顶");
        assert_eq!(l.offset_for_thumb_top(px(9999.0)), 0, "拖过底 = 回到底部");
    }

    #[test]
    fn 命中判定分滑块与轨道() {
        let s = ScrollbarStyle::default();
        let l = layout(bounds(400.0, 300.0), &s, 100, 30, 0).unwrap();
        // 轨道贴右边:400 - 6 - 1 = 393
        assert!((f32::from(l.track.origin.x) - 393.0).abs() < 0.01);
        // 滑块在底部 210..300
        assert_eq!(l.hit(point(px(395.0), px(250.0)), px(3.0)), ScrollbarHit::Thumb);
        assert_eq!(l.hit(point(px(395.0), px(50.0)), px(3.0)), ScrollbarHit::Track);
        // 左边太远 = 交给终端(不能把选择吃掉)
        assert_eq!(l.hit(point(px(300.0), px(50.0)), px(3.0)), ScrollbarHit::Miss);
        // 放宽量让 6px 的条子好点中
        assert_eq!(l.hit(point(px(391.0), px(250.0)), px(3.0)), ScrollbarHit::Thumb);
    }

    #[test]
    fn 点轨道空白翻一页() {
        let s = ScrollbarStyle::default();
        let l = layout(bounds(400.0, 300.0), &s, 100, 30, 0).unwrap();
        // 滑块在底部,点上方 = 往上翻一页(offset 增加一屏)
        assert_eq!(l.offset_for_track_click(point(px(395.0), px(10.0))), 30);
        let l = layout(bounds(400.0, 300.0), &s, 100, 30, 70).unwrap();
        // 滑块在顶部,点下方 = 往下翻一页
        assert_eq!(l.offset_for_track_click(point(px(395.0), px(290.0))), 40);
        // 翻不过头
        let l = layout(bounds(400.0, 300.0), &s, 100, 30, 60).unwrap();
        assert_eq!(l.offset_for_track_click(point(px(395.0), px(5.0))), 70);
    }

    #[test]
    fn 淡出曲线() {
        let s = ScrollbarStyle::default();
        // 拖动/悬停恒亮
        assert_eq!(alpha(&s, Duration::from_secs(60), true, true), 1.0);
        // 延迟内恒亮
        assert_eq!(alpha(&s, Duration::from_millis(500), false, false), 1.0);
        // 中点约在 1 与 floor 的一半处
        let mid = alpha(&s, Duration::from_millis(900 + 225), false, false);
        assert!((mid - (0.5 + 0.5 * 0.5)).abs() < 0.01, "实际 {mid}");
        // 在底部彻底消失,不在底部留一半 —— 「我还在回看」要一直看得见
        assert_eq!(alpha(&s, Duration::from_secs(5), false, true), 0.0);
        assert_eq!(alpha(&s, Duration::from_secs(5), false, false), 0.5);
        // 关掉淡出动画 = 立刻落到静息值
        let instant = ScrollbarStyle {
            fade_duration: Duration::ZERO,
            ..s
        };
        assert_eq!(alpha(&instant, Duration::from_secs(1), false, false), 0.5);
    }

    #[test]
    fn 淡出走完就不再请求动画帧() {
        let s = ScrollbarStyle::default();
        assert!(needs_animation_frame(&s, Duration::from_millis(0), false));
        assert!(needs_animation_frame(&s, Duration::from_millis(1200), false));
        assert!(!needs_animation_frame(&s, Duration::from_millis(1400), false));
        // 手还按着的时候不用自转,鼠标事件自己会带来重绘
        assert!(!needs_animation_frame(&s, Duration::ZERO, true));
    }
}
