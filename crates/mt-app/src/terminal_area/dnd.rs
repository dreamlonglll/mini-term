use super::*;

/// 拖文件悬停时盖在终端上的虚线提示框(`TerminalInstance.tsx:430-442`)。
pub(super) fn drop_hint() -> AnyElement {
    div()
        .absolute()
        .inset(px(4.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .border_2()
        .border_dashed()
        .border_color(ui::accent())
        .bg(ui::accent_subtle())
        .child(
            div()
                .px(px(12.0))
                .py(px(6.0))
                .rounded(px(6.0))
                .bg(ui::bg_overlay())
                .text_size(ui::font_px(9.75))
                .text_color(ui::accent())
                .child(t("terminal", "dropToInsertPath")),
        )
        .into_any_element()
}

/// pane 拖拽的落点预览:半透明 accent 盖住「松手之后这块地归谁」。
///
/// 逐条对照 `PaneGroup.tsx:529-535` 的 `overlayRect`:center 铺满(= 并入本组的
/// tab 栏),四边各占**半屏**(= 往那个方向分屏之后新格子的位置)。
/// 注意四边预览是**半屏**而不是判档用的 1/4 进深 —— 前者画的是结果,后者是手感。
pub(super) fn zone_overlay(zone: DropZone) -> AnyElement {
    let base = div()
        .absolute()
        .border_2()
        .border_color(ui::accent())
        .bg(ui::with_alpha(ui::accent(), 0.2));
    match zone {
        DropZone::Center => base.inset_0(),
        DropZone::Left => base.top_0().bottom_0().left_0().w(gpui::relative(0.5)),
        DropZone::Right => base.top_0().bottom_0().right_0().w(gpui::relative(0.5)),
        DropZone::Top => base.left_0().right_0().top_0().h(gpui::relative(0.5)),
        DropZone::Bottom => base.left_0().right_0().bottom_0().h(gpui::relative(0.5)),
    }
    .into_any_element()
}

impl TerminalArea {
    /// 为什么要在可滚动的 tab 栏**外面**再包一层:
    /// ① 插入指示线是绝对定位的,放进滚动容器里会跟着内容偏移,而 x 又是由
    ///    屏幕坐标现算的(已含滚动量)—— 两下叠加就双算了;包一层非滚动的
    ///    父级,`指示线 x = tab 屏幕左缘 − tab 栏屏幕左缘` 直接可用。
    /// ② `on_drag_move` 的 `event.bounds` 就是挂监听那个元素的矩形,挂在这层
    ///    等于白拿「tab 栏在屏幕上的位置」,不必再为它单开一片量尺 canvas。
    pub(super) fn render_leaf_drop_layer(
        &mut self,
        bar: impl IntoElement,
        project_id: &str,
        leaf_id: &str,
        dragging: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        // ── tab 栏的落点层 ────────────────────────────────────────
        //
        // 为什么要在可滚动的 tab 栏**外面**再包一层:
        // ① 插入指示线是绝对定位的,放进滚动容器里会跟着内容偏移,而 x 又是由
        //    屏幕坐标现算的(已含滚动量)—— 两下叠加就双算了;包一层非滚动的
        //    父级,`指示线 x = tab 屏幕左缘 − tab 栏屏幕左缘` 直接可用。
        // ② `on_drag_move` 的 `event.bounds` 就是挂监听那个元素的矩形,挂在这层
        //    等于白拿「tab 栏在屏幕上的位置」,不必再为它单开一片量尺 canvas。
        let leaf_for_bar = leaf_id.to_string();
        let leaf_for_drop = leaf_id.to_string();
        let pid_tab_drop = project_id.to_string();
        let tab_indicator = self
            .tab_drop
            .as_ref()
            .filter(|(id, _, _)| dragging && id.as_str() == leaf_id)
            .map(|(_, _, x)| *x);
        // ⚠️ 必须是**纵向** flex:`bar` 自己带 `flex_none`,放进默认的横向 flex 里
        // 就变成「宽度按内容撑」,右侧控件簇的 `ml_auto` 会跟着缩到 tab 后面去。
        // 纵向下 `flex_none` 只钉高度(26),宽度照旧横向铺满 —— 与它原先直接
        // 挂在叶子那个 `flex_col` 容器里时的表现一字不差。
        div()
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .on_drag_move(cx.listener({
                let leaf_id = leaf_for_bar.clone();
                move |this: &mut TerminalArea,
                      event: &gpui::DragMoveEvent<crate::dnd::DragPane>,
                      _window,
                      cx| {
                    let dragged = event.drag(cx).clone();
                    this.note_tab_drag_over(
                        &leaf_id,
                        &dragged,
                        event.bounds,
                        event.event.position,
                        cx,
                    );
                }
            }))
            .on_drop(cx.listener({
                let leaf_id = leaf_for_drop.clone();
                move |this: &mut TerminalArea, item: &crate::dnd::DragPane, window, cx| {
                    this.drop_on_tab_bar(&pid_tab_drop, &leaf_id, item, window, cx);
                }
            }))
            .child(bar)
            // 插入位指示线:3px 圆头 + accent 双层光晕(评审定稿口径,见 TAB_DROP_LINE_W)
            .children(tab_indicator.map(|x| {
                div()
                    .absolute()
                    .top(px(2.0))
                    .h(px(22.0))
                    .left(px((x - TAB_DROP_LINE_W / 2.0).max(0.0)))
                    .w(px(TAB_DROP_LINE_W))
                    .rounded_full()
                    .bg(ui::accent())
                    .shadow(vec![
                        gpui::BoxShadow {
                            color: ui::accent(),
                            offset: point(px(0.0), px(0.0)),
                            blur_radius: px(6.0),
                            spread_radius: px(0.0),
                            inset: false,
                        },
                        gpui::BoxShadow {
                            color: ui::accent(),
                            offset: point(px(0.0), px(0.0)),
                            blur_radius: px(2.0),
                            spread_radius: px(0.0),
                            inset: false,
                        },
                    ])
            }))
    }
    // ─── pane 拖拽的四个落点处理(v0.14.0)─────────────────────
    //
    // 见 [`crate::dnd`] 模块注释第 2 条:`on_drag_move` 会打给**每一个**注册者
    // (不只鼠标底下那个),所以四个处理里第一句都是自己的命中判定。

    /// 终端区落点判档。
    ///
    /// **落下没有动作的场景一律不给预览**(原版三轮评审的最终口径,
    /// `PaneGroup.tsx::handleBodyDragMove`):
    /// - 独占一组的 pane 拖回自己身上 —— 四边等价原位、中央本就在同组;
    /// - 拖到自己所在组的中央 —— 已经在这一组里了。
    ///
    /// 这两条与 [`SplitNode::move_pane_in_layout`] 返回 `None` 的条件严格同集,
    /// 所以「有预览 ⟺ 落下有动作」,不会出现指示了却静默无动作。
    pub(super) fn note_pane_drag_over(
        &mut self,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        bounds: Bounds<Pixels>,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let mut next = None;
        if self.accepts_pane_drag(dragged, cx)
            && let Some(zone) = crate::dnd::pane_drop_zone(bounds, position)
            && self.zone_has_effect(leaf_id, dragged, zone, cx)
        {
            next = Some((leaf_id.to_string(), zone));
        }

        // 不命中本组时只收自己那一份,别人的留给别人清
        if next.is_none() && self.pane_drop.as_ref().is_none_or(|(id, _)| id != leaf_id) {
            return;
        }
        if self.pane_drop != next {
            self.pane_drop = next;
            cx.notify();
        }
    }

    /// 终端区落地:读上一帧存下的档位,交给 store 的纯函数变换。
    ///
    /// 锚点用**本组当前激活的 pane**(原版 `movePane(…, node.activePaneId, …)`),
    /// `move_pane_in_layout` 内部会在锚点恰是被拖 pane 时自动换锚。
    pub(super) fn drop_on_pane_body(
        &mut self,
        project_id: &str,
        leaf_id: &str,
        anchor_pane_id: &str,
        dragged: &crate::dnd::DragPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let zone = self
            .pane_drop
            .take()
            .filter(|(id, _)| id == leaf_id)
            .map(|(_, zone)| zone);
        self.pane_drag = None;
        cx.notify();
        let (Some(zone), true) = (zone, self.accepts_pane_drag(dragged, cx)) else {
            return;
        };
        let (pid, pane_id, anchor) = (
            project_id.to_string(),
            dragged.pane_id.clone(),
            anchor_pane_id.to_string(),
        );
        self.store.update(cx, |store, cx| {
            store.move_pane(&pid, &pane_id, &anchor, zone, window, cx);
        });
    }

    /// tab 栏插入位判定。
    ///
    /// 本组只有被拖的这一个 tab 时**不给指示线** —— 组内换位无意义,落下也不会
    /// 有动作,画了就是「指示了却静默无动作」的口径不一致(原版评审结论)。
    pub(super) fn note_tab_drag_over(
        &mut self,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        bounds: Bounds<Pixels>,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let mut next = None;
        if bounds.contains(&position)
            && self.accepts_pane_drag(dragged, cx)
            && let Some(slots) = self.tab_slots(leaf_id, dragged, cx)
        {
            let (index, line_x) = crate::dnd::tab_insert_index(&slots, f32::from(position.x));
            next = Some((
                leaf_id.to_string(),
                index,
                line_x - f32::from(bounds.origin.x),
            ));
        }

        if next.is_none() && self.tab_drop.as_ref().is_none_or(|(id, ..)| id != leaf_id) {
            return;
        }
        if self.tab_drop != next {
            self.tab_drop = next;
            cx.notify();
        }
    }

    /// tab 栏落地:按插入位落子。
    ///
    /// 锚点必须是**本组里不是被拖 pane 的那一个**:它只用来定位目标叶子,
    /// 找不到就说明本组只有被拖的这一个 tab,组内换位无意义(与判档同一道闸)。
    pub(super) fn drop_on_tab_bar(
        &mut self,
        project_id: &str,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let index = self
            .tab_drop
            .take()
            .filter(|(id, ..)| id == leaf_id)
            .map(|(_, index, _)| index);
        self.pane_drag = None;
        cx.notify();
        let (Some(index), true) = (index, self.accepts_pane_drag(dragged, cx)) else {
            return;
        };
        let Some(anchor) = self
            .store
            .read(cx)
            .project_state(project_id)
            .and_then(|s| s.active_layout())
            .and_then(|l| l.node(leaf_id))
            .and_then(|node| match node {
                SplitNode::Leaf { panes, .. } => panes
                    .iter()
                    .find(|p| p.id != dragged.pane_id)
                    .map(|p| p.id.clone()),
                _ => None,
            })
        else {
            return;
        };
        let (pid, pane_id) = (project_id.to_string(), dragged.pane_id.clone());
        self.store.update(cx, |store, cx| {
            store.move_pane_to_tab(&pid, &pane_id, &anchor, index, window, cx);
        });
    }

    /// 本组能不能接这次 pane 拖拽(原版 `acceptsPaneDrag`):只接**同项目**的。
    pub(super) fn accepts_pane_drag(&self, dragged: &crate::dnd::DragPane, cx: &App) -> bool {
        self.store.read(cx).active_project_id.as_deref() == Some(dragged.project_id.as_str())
    }

    /// 这一档落下去会不会真有动作 —— 判据与 [`SplitNode::move_pane_in_layout`]
    /// 返回 `None` 的条件严格同集(见 [`Self::note_pane_drag_over`] 的说明)。
    pub(super) fn zone_has_effect(
        &self,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        zone: DropZone,
        cx: &App,
    ) -> bool {
        let store = self.store.read(cx);
        let Some(layout) = store
            .project_state(&dragged.project_id)
            .and_then(|s| s.active_layout())
        else {
            return false;
        };
        let Some(SplitNode::Leaf { panes, .. }) = layout.node(leaf_id) else {
            return false;
        };
        if !panes.iter().any(|p| p.id == dragged.pane_id) {
            // 别的组:哪一档都有动作
            return true;
        }
        // 自己就在这一组里:独占一组时四档全是空操作,多 tab 时只有中央是
        !(panes.len() == 1 || zone == DropZone::Center)
    }

    /// 本组各 tab 的横向区间(屏幕坐标,按 tab 顺序)。
    ///
    /// 返回 `None` = 这一组不该给指示线:布局里找不到、或组里只有被拖的这一个 tab。
    /// 矩形取自 [`Self::tab_rects`](每个 tab 挂的量尺 canvas),等价于原版
    /// `bar.querySelectorAll('[data-pane-tab]')` 那一趟。
    pub(super) fn tab_slots(
        &self,
        leaf_id: &str,
        dragged: &crate::dnd::DragPane,
        cx: &App,
    ) -> Option<Vec<(f32, f32)>> {
        let store = self.store.read(cx);
        let layout = store
            .project_state(&dragged.project_id)
            .and_then(|s| s.active_layout())?;
        let SplitNode::Leaf { panes, .. } = layout.node(leaf_id)? else {
            return None;
        };
        if !panes.iter().any(|p| p.id != dragged.pane_id) {
            return None;
        }
        // 有一个 tab 还没量到就整份作废:区间与 tab 顺序必须**一一对应**,
        // 缺一个会让后面所有的插入位错一格(比不给指示线糟得多)
        panes
            .iter()
            .map(|p| {
                self.tab_rects
                    .get(&p.id)
                    .map(|r| (f32::from(r.origin.x), f32::from(r.origin.x + r.size.width)))
            })
            .collect()
    }

    /// `on_drag_move` 的落点记录。见 [`crate::dnd`] 模块注释第 2 条:这个回调会
    /// 打给**每一个**注册者(不只鼠标底下那个),命中判定必须自己做。
    pub(super) fn note_file_drag_over(
        &mut self,
        pane_id: &str,
        bounds: Bounds<Pixels>,
        position: gpui::Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let hit = bounds.contains(&position);
        let next = if hit {
            Some(pane_id.to_string())
        } else if self.file_drop_pane.as_deref() == Some(pane_id) {
            // 只收自己那一份,别人的留给别人清
            None
        } else {
            return;
        };
        if self.file_drop_pane != next {
            self.file_drop_pane = next;
            cx.notify();
        }
    }

    /// 把路径文本当作用户键入写进 pane,并把键盘还给终端(原版 `term.focus()`)。
    pub(super) fn insert_path_into_pane(
        &mut self,
        project_id: &str,
        pane_id: &str,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.file_drop_pane = None;
        if text.is_empty() {
            cx.notify();
            return;
        }
        self.store.update(cx, |store, cx| {
            store.write_to_pane(project_id, pane_id, text, cx);
            store.focus_pane(project_id, pane_id, window, cx);
        });
        cx.notify();
    }
}
