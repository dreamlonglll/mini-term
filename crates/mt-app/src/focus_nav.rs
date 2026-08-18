//! 分屏之间的方向导航(`src/utils/layoutOps.ts` 的 `findAdjacentPtyId`)。
//!
//! **按屏幕几何找相邻 pane,而不是在树上推方向**:树形结构里「右边」可能跨好几层
//! split,几何最近邻既简单又与用户所见一致。旧版从 DOM 的 `getBoundingClientRect`
//! 取矩形,这里从 gpui 的元素 bounds 取 —— 打分公式一字不改。

/// 几何方向。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// 一个 pane 在屏幕上的矩形(逻辑像素)。
#[derive(Clone, Debug, PartialEq)]
pub struct PaneRect {
    pub pane_id: String,
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
}

impl PaneRect {
    fn center(&self) -> (f32, f32) {
        (self.left + self.width / 2.0, self.top + self.height / 2.0)
    }
}

/// 打分 = 主轴距离 + 交叉轴错位惩罚(×2),取同方向上得分最低的那个。
///
/// 「必须确实在该方向上」留 1px 容差,避免等宽分屏的浮点边界把正对面那格判掉。
pub fn adjacent_pane(rects: &[PaneRect], from_pane_id: &str, dir: Direction) -> Option<String> {
    let from = rects
        .iter()
        .find(|r| r.pane_id == from_pane_id && r.width > 0.0 && r.height > 0.0)?;
    let (from_cx, from_cy) = from.center();

    let mut best: Option<(&str, f32)> = None;
    for rect in rects {
        if rect.pane_id == from_pane_id || rect.width <= 0.0 || rect.height <= 0.0 {
            continue;
        }
        let (cx, cy) = rect.center();
        let dx = cx - from_cx;
        let dy = cy - from_cy;
        let (main, cross) = match dir {
            Direction::Left => (-dx, dy.abs()),
            Direction::Right => (dx, dy.abs()),
            Direction::Up => (-dy, dx.abs()),
            Direction::Down => (dy, dx.abs()),
        };
        if main <= 1.0 {
            continue;
        }
        let score = main + cross * 2.0;
        if best.map(|(_, s)| score < s).unwrap_or(true) {
            best = Some((rect.pane_id.as_str(), score));
        }
    }
    best.map(|(id, _)| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(id: &str, left: f32, top: f32, w: f32, h: f32) -> PaneRect {
        PaneRect {
            pane_id: id.to_string(),
            left,
            top,
            width: w,
            height: h,
        }
    }

    /// 左右两格:向右到右格,向左回左格,上下没有邻居。
    #[test]
    fn 左右分屏的四向导航() {
        let rects = vec![rect("a", 0.0, 0.0, 500.0, 800.0), rect("b", 500.0, 0.0, 500.0, 800.0)];
        assert_eq!(adjacent_pane(&rects, "a", Direction::Right).as_deref(), Some("b"));
        assert_eq!(adjacent_pane(&rects, "b", Direction::Left).as_deref(), Some("a"));
        assert_eq!(adjacent_pane(&rects, "a", Direction::Up), None);
        assert_eq!(adjacent_pane(&rects, "a", Direction::Down), None);
    }

    /// 上下两格。
    #[test]
    fn 上下分屏的导航() {
        let rects = vec![rect("a", 0.0, 0.0, 800.0, 300.0), rect("b", 0.0, 300.0, 800.0, 300.0)];
        assert_eq!(adjacent_pane(&rects, "a", Direction::Down).as_deref(), Some("b"));
        assert_eq!(adjacent_pane(&rects, "b", Direction::Up).as_deref(), Some("a"));
    }

    /// 田字格:向右取正对面那格,而不是斜对角(交叉轴惩罚 ×2 的作用)。
    #[test]
    fn 田字格向右取正对面而非斜对角() {
        let rects = vec![
            rect("tl", 0.0, 0.0, 400.0, 300.0),
            rect("tr", 400.0, 0.0, 400.0, 300.0),
            rect("bl", 0.0, 300.0, 400.0, 300.0),
            rect("br", 400.0, 300.0, 400.0, 300.0),
        ];
        assert_eq!(adjacent_pane(&rects, "tl", Direction::Right).as_deref(), Some("tr"));
        assert_eq!(adjacent_pane(&rects, "tl", Direction::Down).as_deref(), Some("bl"));
        assert_eq!(adjacent_pane(&rects, "br", Direction::Left).as_deref(), Some("bl"));
        assert_eq!(adjacent_pane(&rects, "br", Direction::Up).as_deref(), Some("tr"));
    }

    /// 跨层 split:右边一列被再切成上下两格时,从左格向右应落到几何上更近的那个。
    #[test]
    fn 跨层分屏取几何最近的那格() {
        let rects = vec![
            rect("left", 0.0, 0.0, 500.0, 600.0),
            rect("right-top", 500.0, 0.0, 500.0, 100.0),
            rect("right-bottom", 500.0, 100.0, 500.0, 500.0),
        ];
        // left 的中心 y=300;right-bottom 中心 y=350(错位 50)胜过 right-top(错位 250)
        assert_eq!(
            adjacent_pane(&rects, "left", Direction::Right).as_deref(),
            Some("right-bottom")
        );
    }

    /// 零尺寸的格子(还没布局出来 / 隐藏)不参与,起点缺失时不动。
    #[test]
    fn 零尺寸与缺失起点都不动() {
        let rects = vec![rect("a", 0.0, 0.0, 500.0, 800.0), rect("b", 500.0, 0.0, 0.0, 0.0)];
        assert_eq!(adjacent_pane(&rects, "a", Direction::Right), None);
        assert_eq!(adjacent_pane(&rects, "missing", Direction::Right), None);
    }

    /// 完全重叠(容差之内)不算「在该方向上」。
    #[test]
    fn 完全重叠不算相邻() {
        let rects = vec![rect("a", 0.0, 0.0, 500.0, 800.0), rect("b", 0.5, 0.0, 500.0, 800.0)];
        assert_eq!(adjacent_pane(&rects, "a", Direction::Right), None);
    }
}
