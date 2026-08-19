//! 提交历史拓扑图的布局算法。逐条移植 `src/utils/gitGraph.ts`(213 行)。
//!
//! # 与 TS 版的两处形式差异(语义一字不差)
//!
//! 1. **颜色是调色板下标而不是色串**。TS 里 `color: string` 的全部用法只有两种:
//!    比较相等(`endColor !== color` 决定要不要渐变)与喂给 SVG。下标同样满足这两条,
//!    还免掉了字符串比较与解析 —— [`palette_color`] 在渲染时才换成 `Hsla`。
//! 2. **`segmentPath` 返回几何而不是 SVG path 串**。gpui 没有 SVG,画线走
//!    [`gpui::PathBuilder`],要的是端点与控制点本身,见 [`SegPath`]。
//!
//! # 为什么行高必须固定 48px
//!
//! 连线要跨行接续:上一行的下半程止于底边某条 lane,下一行的上半程从顶边同一条
//! lane 起步。行高一变,两段接不上。原注释就在 `gitGraph.ts:17`。

use mt_project::git::GitCommitInfo;

/// 单个 lane 的水平间距(px)。
pub const GRAPH_LANE_WIDTH: f32 = 14.0;
/// 每个 commit 行的固定高度(px)。
pub const GRAPH_ROW_HEIGHT: f32 = 48.0;
/// 最多渲染的 lane 数,超出的一律画在最后一列。
pub const GRAPH_MAX_LANES: usize = 8;

/// 8 色循环调色板(`gitGraph.ts:21-30`,逐值照抄)。
pub const PALETTE: [(u8, u8, u8); 8] = [
    (0x58, 0xa6, 0xff),
    (0x3f, 0xb9, 0x50),
    (0xd2, 0x99, 0x22),
    (0xbc, 0x8c, 0xff),
    (0xf7, 0x81, 0x66),
    (0x39, 0xc5, 0xcf),
    (0xdb, 0x61, 0xa2),
    (0xa5, 0xd6, 0xff),
];

/// 调色板下标 → 颜色。越界按 TS 的 `% length` 同款折回。
pub fn palette_color(index: u8) -> gpui::Hsla {
    let (r, g, b) = PALETTE[index as usize % PALETTE.len()];
    mt_ui::rgb8(r, g, b)
}

/// 一条线段。`from`/`to` 为 `-1` 时表示该端点是**本行节点**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphSegment {
    /// 线段在本行顶边所处的 lane;-1 = 从本行节点出发(只画下半程)。
    pub from: i32,
    /// 线段在本行底边所处的 lane;-1 = 终止于本行节点(只画上半程)。
    pub to: i32,
    pub color: u8,
    /// 末端要融入的颜色。与 `color` 相同(或 `None`)时不渐变。
    pub end_color: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphRow {
    /// 节点所在 lane。
    pub lane: usize,
    pub color: u8,
    /// 合并提交(父数 ≥ 2)画空心圆以示区分。
    pub is_merge: bool,
    pub segments: Vec<GraphSegment>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphLayout {
    /// 与传入 commits 一一对应。
    pub rows: Vec<GraphRow>,
    /// 图形区宽度(px)。
    pub width: f32,
}

/// 拓扑布局。输入按拓扑序(父提交永远排在子提交之后)。
///
/// 自上而下扫描,维护一组 lane,每条 lane 记录它当前正在等待的 commit hash。
pub fn compute(commits: &[GitCommitInfo]) -> GraphLayout {
    #[derive(Clone)]
    struct Lane {
        hash: String,
        color: u8,
    }

    let mut lanes: Vec<Option<Lane>> = Vec::new();
    let mut rows: Vec<GraphRow> = Vec::new();
    let mut color_seq: usize = 0;
    let mut max_lane: i32 = 0;

    for commit in commits {
        let mut segments: Vec<GraphSegment> = Vec::new();

        // 1. 找出所有正等待本 commit 的 lane
        let incoming: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_ref().is_some_and(|l| l.hash == commit.hash))
            .map(|(i, _)| i)
            .collect();

        // 2. 节点落在最左侧的那条 incoming lane;没有则新开一条(分支尖端)
        let (lane, color) = if let Some(&first) = incoming.first() {
            let color = lanes[first].as_ref().expect("incoming lane 必然非空").color;
            (first, color)
        } else {
            let idx = alloc_lane(&mut lanes);
            let color = next_color(&mut color_seq);
            lanes[idx] = Some(Lane {
                hash: commit.hash.clone(),
                color,
            });
            (idx, color)
        };

        // 3. 与本 commit 无关的 lane 直穿本行
        for (i, slot) in lanes.iter().enumerate() {
            if let Some(l) = slot
                && i != lane
                && !incoming.contains(&i)
            {
                segments.push(GraphSegment {
                    from: i as i32,
                    to: i as i32,
                    color: l.color,
                    end_color: None,
                });
            }
        }

        // 4. 上半程:incoming 的每条线汇入节点;除节点所在 lane 外全部释放
        for &i in &incoming {
            let from_color = lanes[i].as_ref().expect("incoming lane 必然非空").color;
            segments.push(GraphSegment {
                from: i as i32,
                to: -1,
                color: from_color,
                end_color: Some(color),
            });
            if i != lane {
                lanes[i] = None;
            }
        }

        // 5. 下半程:把父提交派发回 lane。先释放自己这条 lane,等第 0 个父认领。
        lanes[lane] = None;
        let parents = &commit.parent_hashes;
        for (pi, parent) in parents.iter().enumerate() {
            // 该父提交已经有线在等它 → 本行直接汇过去,不另开 lane
            let existing = lanes
                .iter()
                .position(|l| l.as_ref().is_some_and(|l| &l.hash == parent));
            if let Some(existing) = existing {
                // 用本节点的颜色而非目标 lane 的颜色 —— 线的颜色跟着分支走,
                // 一条分支线从诞生到汇入主线全程保持自己的颜色,只在根部渐变融入主线。
                segments.push(GraphSegment {
                    from: -1,
                    to: existing as i32,
                    color,
                    end_color: Some(lanes[existing].as_ref().expect("刚查到").color),
                });
                continue;
            }
            let (target, c) = if pi == 0 {
                (lane, color)
            } else {
                let idx = alloc_lane(&mut lanes);
                (idx, next_color(&mut color_seq))
            };
            lanes[target] = Some(Lane {
                hash: parent.clone(),
                color: c,
            });
            segments.push(GraphSegment {
                from: -1,
                to: target as i32,
                color: c,
                end_color: None,
            });
        }
        // parents 为空(根提交)时 lanes[lane] 保持 None,线到此为止

        let mut row_max = lane as i32;
        for s in &segments {
            row_max = row_max.max(s.from).max(s.to);
        }
        max_lane = max_lane.max(row_max);

        rows.push(GraphRow {
            lane,
            color,
            is_merge: parents.len() >= 2,
            segments,
        });
    }

    let lane_count = ((max_lane + 1) as usize).min(GRAPH_MAX_LANES);
    GraphLayout {
        rows,
        width: lane_count as f32 * GRAPH_LANE_WIDTH + 4.0,
    }
}

fn next_color(seq: &mut usize) -> u8 {
    let c = (*seq % PALETTE.len()) as u8;
    *seq += 1;
    c
}

fn alloc_lane<T>(lanes: &mut Vec<Option<T>>) -> usize {
    if let Some(idx) = lanes.iter().position(Option::is_none) {
        return idx;
    }
    lanes.push(None);
    lanes.len() - 1
}

/// lane 索引 → 图形区内 x 坐标(lane 中心)。
pub fn lane_x(lane: i32) -> f32 {
    let clamped = lane.min(GRAPH_MAX_LANES as i32 - 1).max(0);
    clamped as f32 * GRAPH_LANE_WIDTH + GRAPH_LANE_WIDTH / 2.0
}

/// 贝塞尔控制点到端点的垂直距离(`gitGraph.ts:155`:`GRAPH_ROW_HEIGHT / 4`)。
const CURVE: f32 = GRAPH_ROW_HEIGHT / 4.0;

/// 一条线段编译出来的几何(替 TS 侧的 SVG path 串)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SegPath {
    /// 竖直线段(TS 的 `M x y0 V y1`)。
    Line { x: f32, y0: f32, y1: f32 },
    /// 三次贝塞尔(TS 的 `M x0 y0 C c1 c2 x1 y1`),两端切线都是垂直的。
    Cubic {
        p0: (f32, f32),
        c1: (f32, f32),
        c2: (f32, f32),
        p1: (f32, f32),
    },
}

/// 把一条线段编译成几何。逐条对应 `segmentPath`(`gitGraph.ts:158-187`)。
pub fn segment_path(seg: &GraphSegment, node_lane: usize) -> Option<SegPath> {
    let h = GRAPH_ROW_HEIGHT;
    let mid = h / 2.0;

    // 直穿整行
    if seg.from >= 0 && seg.to >= 0 {
        let xf = lane_x(seg.from);
        let xt = lane_x(seg.to);
        if xf == xt {
            return Some(SegPath::Line {
                x: xf,
                y0: 0.0,
                y1: h,
            });
        }
        return Some(SegPath::Cubic {
            p0: (xf, 0.0),
            c1: (xf, mid),
            c2: (xt, mid),
            p1: (xt, h),
        });
    }

    let xn = lane_x(node_lane as i32);

    // 上半程:从顶边的某条 lane 汇入节点
    if seg.from >= 0 {
        let xf = lane_x(seg.from);
        if xf == xn {
            return Some(SegPath::Line {
                x: xf,
                y0: 0.0,
                y1: mid,
            });
        }
        return Some(SegPath::Cubic {
            p0: (xf, 0.0),
            c1: (xf, CURVE),
            c2: (xn, CURVE),
            p1: (xn, mid),
        });
    }

    // 下半程:从节点分出到底边的某条 lane
    if seg.to >= 0 {
        let xt = lane_x(seg.to);
        if xt == xn {
            return Some(SegPath::Line {
                x: xn,
                y0: mid,
                y1: h,
            });
        }
        return Some(SegPath::Cubic {
            p0: (xn, mid),
            c1: (xn, mid + CURVE),
            c2: (xt, mid + CURVE),
            p1: (xt, h),
        });
    }

    None
}

/// 线段是否需要渐变(两端异色才需要)。
pub fn needs_gradient(seg: &GraphSegment) -> bool {
    matches!(seg.end_color, Some(end) if end != seg.color)
}

#[cfg(test)]
pub(crate) fn test_commit(hash: &str, parents: &[&str]) -> GitCommitInfo {
    GitCommitInfo {
        hash: hash.to_string(),
        short_hash: hash.chars().take(7).collect(),
        message: format!("msg {hash}"),
        body: None,
        author: "tester".into(),
        timestamp: 0,
        parent_hashes: parents.iter().map(|p| p.to_string()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(from: i32, to: i32, color: u8, end_color: Option<u8>) -> GraphSegment {
        GraphSegment {
            from,
            to,
            color,
            end_color,
        }
    }

    /// 单条直线历史:一条 lane 从头用到尾,颜色只分配一次。
    #[test]
    fn 线性历史只占一条_lane() {
        let commits = [
            test_commit("c3", &["c2"]),
            test_commit("c2", &["c1"]),
            test_commit("c1", &[]),
        ];
        let layout = compute(&commits);
        assert_eq!(layout.rows.len(), 3);
        for row in &layout.rows {
            assert_eq!(row.lane, 0);
            assert_eq!(row.color, 0, "整条线共用第一个调色板色");
            assert!(!row.is_merge);
        }
        // 第一行:分支尖端,没有 incoming,只有一条下半程
        assert_eq!(layout.rows[0].segments, vec![seg(-1, 0, 0, None)]);
        // 中间行:一条上半程 + 一条下半程
        assert_eq!(
            layout.rows[1].segments,
            vec![seg(0, -1, 0, Some(0)), seg(-1, 0, 0, None)]
        );
        // 根提交:只有上半程,线到此为止
        assert_eq!(layout.rows[2].segments, vec![seg(0, -1, 0, Some(0))]);
        // 单 lane → width = 1*14 + 4
        assert_eq!(layout.width, 18.0);
    }

    /// 两个分支尖端各自开 lane 并各拿一个颜色,汇到共同父提交时第二条 lane 释放。
    #[test]
    fn 两条分支尖端各开一条_lane_并在共同父处收拢() {
        let commits = [
            test_commit("a", &["base"]),
            test_commit("b", &["base"]),
            test_commit("base", &[]),
        ];
        let layout = compute(&commits);
        assert_eq!(layout.rows[0].lane, 0);
        assert_eq!(layout.rows[0].color, 0);
        // b 没有 lane 在等它 → 新开 lane 1 + 下一个颜色
        assert_eq!(layout.rows[1].lane, 1);
        assert_eq!(layout.rows[1].color, 1);
        // b 这一行:lane0 直穿(它在等 base);b 自己**没有上半程**——它是分支尖端,
        // 没有 lane 在等它;下半程的父已经有 lane0 在等 → 汇过去而不另开 lane,
        // 颜色用 b 自己的、末端融入 lane0 的色
        assert_eq!(
            layout.rows[1].segments,
            vec![seg(0, 0, 0, None), seg(-1, 0, 1, Some(0))]
        );
        // base 落在最左的 incoming lane(0)上,继承它的颜色
        assert_eq!(layout.rows[2].lane, 0);
        assert_eq!(layout.rows[2].color, 0);
        // 两条 lane → width = 2*14 + 4
        assert_eq!(layout.width, 32.0);
    }

    /// 合并提交:父数 ≥ 2 → `is_merge`,第 1 个父另开 lane 并拿新颜色。
    #[test]
    fn 合并提交派发两条父线() {
        let commits = [
            test_commit("m", &["p0", "p1"]),
            test_commit("p0", &["root"]),
            test_commit("p1", &["root"]),
            test_commit("root", &[]),
        ];
        let layout = compute(&commits);
        assert!(layout.rows[0].is_merge, "父数 2 → 空心圆");
        assert!(!layout.rows[1].is_merge);
        // m 的下半程:p0 继承节点 lane 与颜色,p1 另开 lane 1 + 下一个颜色
        assert_eq!(
            layout.rows[0].segments,
            vec![seg(-1, 0, 0, None), seg(-1, 1, 1, None)]
        );
        // p1 在 lane1、颜色 1;它的父 root 已被 lane0 认领 → 汇过去
        assert_eq!(layout.rows[2].lane, 1);
        assert_eq!(layout.rows[2].color, 1);
        assert_eq!(
            layout.rows[2].segments,
            vec![
                seg(0, 0, 0, None),
                seg(1, -1, 1, Some(1)),
                seg(-1, 0, 1, Some(0)),
            ]
        );
    }

    /// 释放出来的 lane 会被后面的分支尖端复用(`allocLane` 找第一个 null)。
    #[test]
    fn 释放的_lane_会被复用() {
        let commits = [
            test_commit("a", &["x"]),
            test_commit("b", &["x"]),
            test_commit("x", &[]),
            // x 是根提交,两条 lane 都空了 → c 复用 lane 0
            test_commit("c", &[]),
        ];
        let layout = compute(&commits);
        assert_eq!(layout.rows[3].lane, 0);
        // 颜色是按分配次序取的第 3 个(a=0, b=1, c=2)
        assert_eq!(layout.rows[3].color, 2);
    }

    /// 调色板 8 色循环:第 9 次分配回到第 0 色。
    #[test]
    fn 调色板八色循环() {
        let commits: Vec<_> = (0..9)
            .map(|i| test_commit(&format!("t{i}"), &[]))
            .collect();
        let layout = compute(&commits);
        let colors: Vec<u8> = layout.rows.iter().map(|r| r.color).collect();
        assert_eq!(colors, vec![0, 1, 2, 3, 4, 5, 6, 7, 0]);
    }

    /// 宽度封顶在 8 条 lane(超出的一律画在最后一列)。
    #[test]
    fn 宽度封顶在八条_lane() {
        // 12 个互不相干的尖端(都是根提交,画完即释放 lane)——
        // 每行都只占 lane 0,宽度应当停在 1 条 lane
        let roots: Vec<_> = (0..12).map(|i| test_commit(&format!("r{i}"), &[])).collect();
        assert_eq!(compute(&roots).width, GRAPH_LANE_WIDTH + 4.0);

        // 12 条同时活着的线:第一行开 12 条 lane,宽度按 8 封顶
        let mut commits = vec![test_commit(
            "m",
            &[
                "p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9", "pa", "pb",
            ],
        )];
        for i in 0..12 {
            let name = ["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9", "pa", "pb"][i];
            commits.push(test_commit(name, &[]));
        }
        let layout = compute(&commits);
        assert_eq!(
            layout.width,
            GRAPH_MAX_LANES as f32 * GRAPH_LANE_WIDTH + 4.0
        );
    }

    /// `laneX` 在第 8 条以后夹住(所有溢出 lane 画在同一列)。
    #[test]
    fn lane_x_溢出后夹在最后一列() {
        assert_eq!(lane_x(0), 7.0);
        assert_eq!(lane_x(1), 21.0);
        assert_eq!(lane_x(7), 7.0 * 14.0 + 7.0);
        assert_eq!(lane_x(9), lane_x(7), "第 8 条以后全画在最后一列");
        assert_eq!(lane_x(-1), lane_x(0), "-1 是节点标记,不该产生负坐标");
    }

    /// 路径编译:同 lane 走直线、异 lane 走贝塞尔,控制点位置逐条对表。
    #[test]
    fn 路径编译三种形态() {
        // 直穿同 lane
        assert_eq!(
            segment_path(&seg(1, 1, 0, None), 0),
            Some(SegPath::Line {
                x: 21.0,
                y0: 0.0,
                y1: 48.0
            })
        );
        // 直穿异 lane:控制点在行中线
        assert_eq!(
            segment_path(&seg(0, 1, 0, None), 2),
            Some(SegPath::Cubic {
                p0: (7.0, 0.0),
                c1: (7.0, 24.0),
                c2: (21.0, 24.0),
                p1: (21.0, 48.0),
            })
        );
        // 上半程异 lane:控制点在 CURVE(12)
        assert_eq!(
            segment_path(&seg(1, -1, 0, None), 0),
            Some(SegPath::Cubic {
                p0: (21.0, 0.0),
                c1: (21.0, 12.0),
                c2: (7.0, 12.0),
                p1: (7.0, 24.0),
            })
        );
        // 下半程同 lane
        assert_eq!(
            segment_path(&seg(-1, 0, 0, None), 0),
            Some(SegPath::Line {
                x: 7.0,
                y0: 24.0,
                y1: 48.0
            })
        );
        // 下半程异 lane:控制点在 mid + CURVE(36)
        assert_eq!(
            segment_path(&seg(-1, 1, 0, None), 0),
            Some(SegPath::Cubic {
                p0: (7.0, 24.0),
                c1: (7.0, 36.0),
                c2: (21.0, 36.0),
                p1: (21.0, 48.0),
            })
        );
        // 两端都是 -1(不会出现)→ 无路径
        assert_eq!(segment_path(&seg(-1, -1, 0, None), 0), None);
    }

    /// 渐变判定:只有两端异色才渐变。
    #[test]
    fn 渐变只在两端异色时需要() {
        assert!(!needs_gradient(&seg(0, 1, 3, None)));
        assert!(!needs_gradient(&seg(0, -1, 3, Some(3))));
        assert!(needs_gradient(&seg(0, -1, 3, Some(5))));
    }

    /// 空输入不炸,宽度是 1 条 lane 的最小值(`maxLane` 初值 0)。
    #[test]
    fn 空提交列表() {
        let layout = compute(&[]);
        assert!(layout.rows.is_empty());
        assert_eq!(layout.width, GRAPH_LANE_WIDTH + 4.0);
    }
}
