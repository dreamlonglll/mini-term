use super::*;

/// 某个叶子在整树铺满 `(w, h)` 时占的矩形(相对终端区原点)。
/// 与 [`TerminalArea::render_node`] 同一套比例换算([`split_fractions`]),
/// 还原动画的目标矩形靠它 —— 还原那一帧整树还没画,`pane_rects` 里没有现成值。
pub(super) fn leaf_rect_in(
    node: &SplitNode,
    leaf_id: &str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Option<RevealFrom> {
    node_rect_in(node, leaf_id, x, y, w, h)
}

/// [`leaf_rect_in`] 的通用版:任意节点(叶子或 split)的矩形。双击分隔条认下标时要
/// split 自己的起点(见 [`nearest_boundary`])。节点 id 全树唯一,叶子与 split 不会撞。
pub(super) fn node_rect_in(
    node: &SplitNode,
    node_id: &str,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Option<RevealFrom> {
    if node.id() == node_id {
        return Some(RevealFrom {
            left: x,
            top: y,
            width: w,
            height: h,
        });
    }
    let SplitNode::Split {
        direction,
        children,
        sizes,
        ..
    } = node
    else {
        return None;
    };
    let fractions = split_fractions(sizes, children.len());
    let mut off = 0.0f32;
    for (i, child) in children.iter().enumerate() {
        let f = fractions.get(i).copied().unwrap_or(0.0) as f32;
        let found = if *direction == SplitDirection::Horizontal {
            node_rect_in(child, node_id, x + off * w, y, w * f, h)
        } else {
            node_rect_in(child, node_id, x, y + off * h, w, h * f)
        };
        if found.is_some() {
            return found;
        }
        off += f;
    }
    None
}
/// 折叠标题条区最多吃掉终端区多高。叶子多到码不下时那一区自己滚,
/// **绝不挤掉铺满的那一格** —— 最大化的本意就是给它腾地方。
pub(super) const COLLAPSED_ZONE_MAX: f32 = 0.4;

/// 折叠区的高度上限(终端区高 × [`COLLAPSED_ZONE_MAX`],至少一条的高)。
pub(super) fn collapsed_zone_max(area_h: f32) -> f32 {
    (area_h * COLLAPSED_ZONE_MAX).max(TAB_BAR_H)
}

/// 铺满态下折叠区的**实际**高度:`bars` 条折叠条按内容撑开,顶到上限就自己滚。
/// 与 [`TerminalArea::render_maximized`] 里 `max_h(上限)` + 每条 `TAB_BAR_H` 的排法同值,
/// 还原飞行层按它占位 —— 差一点,飞行层里的终端就比铺满态高 / 矮,多一次 PTY resize。
pub(super) fn collapsed_zone_height(area_h: f32, bars: usize) -> f32 {
    collapsed_zone_max(area_h).min(bars as f32 * TAB_BAR_H)
}

/// 还原飞行层的折叠条条数:按**还原开始前那一帧**(铺满态真正画出来的)的布局数,
/// 不按变更之后的。
///
/// 最大化状态下分屏(`split_pane_with_cwd`、拖到终端区边缘)会先 `clear_maximized`,
/// 同一帧叶子数 +1 并开始还原 —— 按新布局数,飞行层的折叠区比铺满态多一条、终端主体
/// 矮 `TAB_BAR_H`,还原第一帧多一次 PTY resize。上一帧快照缺席(理论走不到)才退回现布局。
pub(super) fn restore_bar_count(prev: Option<&SplitNode>, now: &SplitNode) -> usize {
    prev.unwrap_or(now).leaves().len().saturating_sub(1)
}
/// 各子节点占主轴的比例(和为 1)。
///
/// 存的百分比与子节点数对不上(旧配置 / 塌陷过一次)时**均分**,与
/// `src/utils/layoutOps.ts` 里「子节点数变化后均分而不是按旧值截断」同一处置。
pub(super) fn split_fractions(sizes: &[f64], count: usize) -> Vec<f64> {
    if count == 0 {
        return Vec::new();
    }
    let usable = sizes.len() == count && sizes.iter().all(|s| s.is_finite() && *s > 0.0);
    if !usable {
        return vec![1.0 / count as f64; count];
    }
    let total: f64 = sizes.iter().sum();
    sizes.iter().map(|s| s / total).collect()
}

/// 分隔条拖完后的像素 → 百分比(和为 100,与磁盘格式同口径)。
///
/// 总和非正(面板还没量出来 / 全被折叠)时返回 `None`,调用方据此**不写回** ——
/// 把一串 0 写进布局树会让下次恢复全部退化成均分。
pub(super) fn sizes_to_percent(pixels: &[f64]) -> Option<Vec<f64>> {
    let total: f64 = pixels.iter().filter(|p| p.is_finite()).sum();
    if total.is_nan() || total <= 0.0 {
        return None;
    }
    Some(pixels.iter().map(|p| p / total * 100.0).collect())
}

// ─── 键盘调比例 / 双击分隔条均分(纯函数)────────────────────────
//
// 两条路最后都落到 `ResizableState::resize_panel`(与拖分隔条同一套规则),再把它算出的
// 像素换回百分比写进 store(与拖完松手同一条 `set_split_sizes` → layout.db 去抖落盘)。
// 这里只算「分隔线该停在哪」:只在分隔线两侧那两格之间挪,两格都不小于组件库的最小格宽
// (`PANEL_MIN_SIZE`)—— 挪不动就不挪,不会把更远的格子连带挤小。

/// 键盘调比例一步挪多少:分隔线所在 split 主轴长度的 5%。
pub(super) const SPLIT_NUDGE_STEP: f32 = 0.05;

/// 第 `boundary` 条分隔线(`sizes[boundary]` 与 `sizes[boundary + 1]` 之间)朝后一格挪
/// (`grow = true`,→ / ↓)或朝前一格挪一步之后,`sizes[boundary]` 该是多宽。
///
/// 两格都钳在 `min` 以上;挪不动(已经顶到最小、两格合起来都放不下两个最小值、容器还没量出来)
/// 或挪动不到半像素时返回 `None`。
pub(super) fn nudged_panel_size(
    sizes: &[f32],
    boundary: usize,
    container: f32,
    grow: bool,
    min: f32,
) -> Option<f32> {
    let (a, b) = (*sizes.get(boundary)?, *sizes.get(boundary + 1)?);
    if !container.is_finite() || container <= 0.0 {
        return None;
    }
    let step = container * SPLIT_NUDGE_STEP;
    let (lo, hi) = (min, a + b - min);
    if hi < lo {
        return None;
    }
    let moved = if grow { a + step } else { a - step };
    let target = moved.clamp(lo, hi);
    ((target - a).abs() >= 0.5).then_some(target)
}

/// 双击第 `boundary` 条分隔线:两侧两格均分,返回 `sizes[boundary]` 的新宽度;
/// 已经等宽(差不到 1px)时返回 `None`。
pub(super) fn equalized_panel_size(sizes: &[f32], boundary: usize) -> Option<f32> {
    let (a, b) = (*sizes.get(boundary)?, *sizes.get(boundary + 1)?);
    ((a - b).abs() >= 1.0).then_some((a + b) / 2.0)
}

/// 主轴上离 `pos` 最近的那条分隔线的下标。`start` 是 split 在主轴上的起点,`sizes` 是各格
/// 实际像素宽(分隔线 i 落在 `start + sizes[0..=i]` 之和处)。少于两格返回 `None`。
///
/// 双击分隔条时拿它认「点的是哪一条」:组件库给分隔条外观回调时不告诉下标,
/// 只好按点击位置反推 —— 两条分隔线之间至少隔一个最小格宽,不会认错。
pub(super) fn nearest_boundary(sizes: &[f32], start: f32, pos: f32) -> Option<usize> {
    let mut edge = start;
    let mut best: Option<(usize, f32)> = None;
    for (i, size) in sizes.iter().enumerate().take(sizes.len().saturating_sub(1)) {
        edge += size;
        let dist = (pos - edge).abs();
        if best.is_none_or(|(_, d)| dist < d) {
            best = Some((i, dist));
        }
    }
    best.map(|(i, _)| i)
}

/// 分隔条拖拽区单侧的宽度,与组件库分隔条的内衬(gpui-base `HANDLE_PADDING`,crate 私有)
/// 同值:热区要与拖拽区严丝合缝,宽了会抢走旁边格子的点击。
pub(super) const SPLIT_HANDLE_PAD: f32 = 4.0;

/// 一条分隔条画什么,外加「双击均分」的热区。
///
/// 组件库只把**画什么**交给外观回调(`ResizeHandleRenderer`),热区、光标与拖拽仍归它
/// 自己;回调一旦接手就得连那条 1px 线一起画,这里照内置样子画(颜色取法同组件库:静止
/// `border`、拖动中 `drag_border`)。线上挂一块透明热区,铺满分隔条 8px 的拖拽区,只接
/// 双击、不 `occlude`,拖拽照旧由外层分隔条接。与拖拽不打架:起拖的那一帧 gpui 会清掉
/// 热区记着的按下(`pending_mouse_down`),拖完松手不会被认成一次点击;没挪动的按下 /
/// 抬起本来也起不了拖。
pub(super) fn split_handle(
    handle: &gpui_base::ResizeHandleContext,
    on_double_click: impl Fn(gpui::Point<Pixels>, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let horizontal = matches!(handle.axis(), gpui::Axis::Horizontal);
    let theme = cx.theme();
    let color = if handle.is_active() {
        theme.drag_border
    } else {
        theme.border
    };
    let hit = div()
        .id("split-equalize")
        .absolute()
        .map(|el| {
            if horizontal {
                el.top_0()
                    .bottom_0()
                    .left(px(-SPLIT_HANDLE_PAD))
                    .w(px(2.0 * SPLIT_HANDLE_PAD))
                    .cursor_col_resize()
            } else {
                el.left_0()
                    .right_0()
                    .top(px(-SPLIT_HANDLE_PAD))
                    .h(px(2.0 * SPLIT_HANDLE_PAD))
                    .cursor_row_resize()
            }
        })
        .on_click(move |event, window, cx| {
            if click_count(event) < 2 {
                return;
            }
            cx.stop_propagation();
            on_double_click(click_position(event, window), window, cx);
        });
    div()
        .flex_none()
        .bg(color)
        .map(|el| {
            if horizontal {
                el.h_full().w(px(1.0))
            } else {
                el.w_full().h(px(1.0))
            }
        })
        .child(hit)
        .into_any_element()
}

impl TerminalArea {
    pub(super) fn split_state(&mut self, node_id: &str, cx: &mut App) -> Entity<ResizableState> {
        self.split_states
            .entry(node_id.to_string())
            .or_insert_with(|| cx.new(|_| ResizableState::default()))
            .clone()
    }

    /// 把键盘焦点移到相邻分屏(`focusAdjacentPane`)。
    ///
    /// 返回「真的挪了没有」:那个方向没有相邻分屏时是 `false`,调用方据此把 Alt+方向键
    /// 放行给终端(见 `main.rs::Workspace::focus_adjacent`)。
    pub fn focus_adjacent(
        &mut self,
        dir: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(project_id) = self.store.read(cx).active_project_id.clone() else {
            return false;
        };
        let Some(from) = self.store.read(cx).active_pane_id(&project_id) else {
            return false;
        };
        // 只在当前项目的 pane 里挑:别的项目的矩形是上一次渲染留下的残影
        let live: Vec<PaneRect> = {
            let store = self.store.read(cx);
            let Some(layout) = store.active_layout() else {
                return false;
            };
            layout
                .panes()
                .into_iter()
                .filter_map(|p| self.pane_rects.get(&p.id).cloned())
                .collect()
        };
        let Some(target) = focus_nav::adjacent_pane(&live, &from, dir) else {
            return false;
        };
        self.store.update(cx, |store, cx| {
            store.activate_pane(&project_id, &target, window, cx)
        });
        true
    }

    /// 键盘调分屏比例(Alt+Shift+方向键):把当前 pane 在 `dir` 那一侧最近的分隔线朝 `dir`
    /// 推一步([`SPLIT_NUDGE_STEP`]),pane 朝那边变大。
    ///
    /// 返回「那个方向有没有可调的分隔线」:没有(贴着区域边缘 / 单格布局 / 正最大化着、
    /// 其余格子都收在折叠条里 / 分隔条状态还没建出来)时是 `false`,调用方据此把按键放行
    /// 给终端(见 `main.rs::Workspace::resize_split`)。有分隔线但已经顶到最小格宽时照样
    /// 返回 `true` —— 按键归调比例,只是这一下挪不动。
    pub fn resize_split(
        &mut self,
        dir: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (axis, forward) = match dir {
            Direction::Left => (SplitDirection::Horizontal, false),
            Direction::Right => (SplitDirection::Horizontal, true),
            Direction::Up => (SplitDirection::Vertical, false),
            Direction::Down => (SplitDirection::Vertical, true),
        };
        let found = {
            let store = self.store.read(cx);
            store.active_project_id.clone().and_then(|project_id| {
                if store.maximized_pane_id(&project_id).is_some() {
                    return None;
                }
                let pane_id = store.active_pane_id(&project_id)?;
                let (node_id, boundary) = store
                    .active_layout()?
                    .boundary_toward(&pane_id, axis, forward)?;
                Some((project_id, node_id, boundary))
            })
        };
        let Some((project_id, node_id, boundary)) = found else {
            return false;
        };
        let Some(state) = self.split_states.get(&node_id).cloned() else {
            return false;
        };
        let (sizes, container) = {
            let state = state.read(cx);
            (
                state
                    .sizes()
                    .iter()
                    .map(|p| f32::from(*p))
                    .collect::<Vec<_>>(),
                f32::from(state.container_size()),
            )
        };
        let min = f32::from(gpui_base::PANEL_MIN_SIZE);
        if let Some(size) = nudged_panel_size(&sizes, boundary, container, forward, min) {
            self.apply_split_resize(&project_id, &node_id, &state, boundary, size, window, cx);
        }
        true
    }

    /// 双击分隔条:那条分隔线两侧两格均分。`position` 是点击处(窗口坐标),下标按它反推
    /// (组件库的分隔条外观回调不告诉是第几条,见 [`nearest_boundary`])。
    pub(super) fn equalize_split_at(
        &mut self,
        node_id: &str,
        state: &Entity<ResizableState>,
        position: gpui::Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let found = {
            let store = self.store.read(cx);
            store.active_project_id.clone().and_then(|project_id| {
                let layout = store.active_layout()?;
                let horizontal = matches!(
                    layout.node(node_id),
                    Some(SplitNode::Split {
                        direction: SplitDirection::Horizontal,
                        ..
                    })
                );
                let rect = node_rect_in(
                    layout,
                    node_id,
                    0.0,
                    0.0,
                    f32::from(self.area_size.width),
                    f32::from(self.area_size.height),
                )?;
                Some((project_id, horizontal, rect))
            })
        };
        let Some((project_id, horizontal, rect)) = found else {
            return;
        };
        let (start, pos) = if horizontal {
            (rect.left, f32::from(position.x - self.area_origin.x))
        } else {
            (rect.top, f32::from(position.y - self.area_origin.y))
        };
        let sizes: Vec<f32> = state
            .read(cx)
            .sizes()
            .iter()
            .map(|p| f32::from(*p))
            .collect();
        let Some(boundary) = nearest_boundary(&sizes, start, pos) else {
            return;
        };
        if let Some(size) = equalized_panel_size(&sizes, boundary) {
            self.apply_split_resize(&project_id, node_id, state, boundary, size, window, cx);
        }
    }

    /// 让第 `boundary` 条分隔线停到「前一格宽 `size`」处:交给 `ResizableState::resize_panel`
    /// (与拖分隔条同一套钳位规则),再把它算出的像素换回百分比写进 store —— 与拖完松手的
    /// `on_resize` 同一条落盘路径(`set_split_sizes` → layout.db 去抖)。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn apply_split_resize(
        &mut self,
        project_id: &str,
        node_id: &str,
        state: &Entity<ResizableState>,
        boundary: usize,
        size: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        state.update(cx, |state, cx| {
            state.resize_panel(boundary, px(size), window, cx)
        });
        let sizes: Vec<f64> = state
            .read(cx)
            .sizes()
            .iter()
            .map(|p| f32::from(*p) as f64)
            .collect();
        if let Some(pct) = sizes_to_percent(&sizes) {
            self.store.update(cx, |store, cx| {
                store.set_split_sizes(project_id, node_id, pct, cx)
            });
        }
        cx.notify();
    }
}
