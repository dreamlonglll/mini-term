//! 分屏之间的方向导航(`src/utils/layoutOps.ts` 的 `findAdjacentPtyId`)。
//!
//! **按屏幕几何找相邻 pane,而不是在树上推方向**:树形结构里「右边」可能跨好几层
//! split,几何最近邻既简单又与用户所见一致。旧版从 DOM 的 `getBoundingClientRect`
//! 取矩形,这里从 gpui 的元素 bounds 取 —— 打分公式一字不改。
//!
//! 后半是列表行的键盘导航([`row_nav_target`]):文件树与项目列表的 ↑ / ↓ / Home / End,
//! 外加文件树 ← / → 的「回父行 / 进子行」。只算下标,焦点与滚动由各列表自己落。

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

// ─── 列表行的键盘导航 ──────────────────────────────────────────

/// 列表 / 树里的行焦点往哪挪。文件树与项目列表共用(两者的行都是 `tab_index(0)` 的
/// 焦点停靠点,此前只能靠 Tab 逐个走)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowNav {
    Prev,
    Next,
    First,
    Last,
    /// 树:回到父行(← 落在已折叠的目录或文件上)。
    Parent,
    /// 树:进到第一个子行(→ 落在已展开的目录上)。
    FirstChild,
}

/// 两种列表共有的四个键(↑ / ↓ / Home / End)。← / → 在树里另有折叠 / 展开的语义,
/// 由文件树自己判。
pub fn row_nav_for_key(key: &str) -> Option<RowNav> {
    match key {
        "up" => Some(RowNav::Prev),
        "down" => Some(RowNav::Next),
        "home" => Some(RowNav::First),
        "end" => Some(RowNav::Last),
        _ => None,
    }
}

/// 焦点该挪到第几行。`depths` 是**当前可见行**按序的缩进层级(扁平列表全给 0 也行,
/// 只有 [`RowNav::Parent`] / [`RowNav::FirstChild`] 看它),`current` 是焦点所在的行。
///
/// 到头停住、不循环(资源管理器 / VS Code 的树都是这样);没有父行 / 子行、或者
/// 目标就是自己时返回 `None`,调用方据此什么都不做。父行按「往上第一个更浅的行」找,
/// 不要求恰好浅一层 —— 拍平的行序里那一行一定就是它的父目录。
pub fn row_nav_target(depths: &[usize], current: usize, nav: RowNav) -> Option<usize> {
    let depth = *depths.get(current)?;
    let target = match nav {
        RowNav::Prev => current.checked_sub(1)?,
        RowNav::Next => current + 1,
        RowNav::First => 0,
        RowNav::Last => depths.len() - 1,
        RowNav::Parent => (0..current).rev().find(|&row| depths[row] < depth)?,
        RowNav::FirstChild => {
            let next = current + 1;
            (*depths.get(next)? > depth).then_some(next)?
        }
    };
    (target < depths.len() && target != current).then_some(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 扁平的四个键:到头停住、不绕圈;目标是自己(已在首行按 Home)也算不动。
    #[test]
    fn 列表上下与首尾到头停住() {
        let flat = [0, 0, 0];
        assert_eq!(row_nav_target(&flat, 1, RowNav::Prev), Some(0));
        assert_eq!(row_nav_target(&flat, 1, RowNav::Next), Some(2));
        assert_eq!(row_nav_target(&flat, 0, RowNav::Prev), None);
        assert_eq!(row_nav_target(&flat, 2, RowNav::Next), None);
        assert_eq!(row_nav_target(&flat, 2, RowNav::First), Some(0));
        assert_eq!(row_nav_target(&flat, 0, RowNav::Last), Some(2));
        assert_eq!(row_nav_target(&flat, 0, RowNav::First), None);
        assert_eq!(row_nav_target(&flat, 2, RowNav::Last), None);
        // 焦点行已不在可见行里(刚被折叠掉):不动
        assert_eq!(row_nav_target(&flat, 3, RowNav::Prev), None);
        assert_eq!(row_nav_target(&[], 0, RowNav::First), None);
    }

    /// 树的 ← / →:回父行跳过中间的兄弟子树,只进「紧跟着的更深一层」。
    #[test]
    fn 树里回父行与进首个子行() {
        // src/            0
        //   main.rs       1
        //   lib/          1
        //     mod.rs      2
        //   util.rs       1
        // README.md       0
        let tree = [0, 1, 1, 2, 1, 0];
        assert_eq!(row_nav_target(&tree, 4, RowNav::Parent), Some(0));
        assert_eq!(row_nav_target(&tree, 3, RowNav::Parent), Some(2));
        assert_eq!(row_nav_target(&tree, 0, RowNav::Parent), None);
        assert_eq!(row_nav_target(&tree, 5, RowNav::Parent), None);
        assert_eq!(row_nav_target(&tree, 0, RowNav::FirstChild), Some(1));
        assert_eq!(row_nav_target(&tree, 2, RowNav::FirstChild), Some(3));
        // 展开着但是空目录 / 还没列完:下一行不更深,不动
        assert_eq!(row_nav_target(&tree, 1, RowNav::FirstChild), None);
        assert_eq!(row_nav_target(&tree, 5, RowNav::FirstChild), None);
    }

    #[test]
    fn 只认四个列表键() {
        assert_eq!(row_nav_for_key("up"), Some(RowNav::Prev));
        assert_eq!(row_nav_for_key("down"), Some(RowNav::Next));
        assert_eq!(row_nav_for_key("home"), Some(RowNav::First));
        assert_eq!(row_nav_for_key("end"), Some(RowNav::Last));
        assert_eq!(row_nav_for_key("left"), None);
        assert_eq!(row_nav_for_key("pageup"), None);
    }

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
