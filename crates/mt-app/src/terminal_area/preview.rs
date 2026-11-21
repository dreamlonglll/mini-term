use super::*;

impl TerminalArea {
    // ─── 非激活 tab 的悬停缩略图(`PaneGroup.tsx:234-277`) ─────

    /// 收起浮层(播收起动画,播完才卸)并取消在飞的计时。
    pub(super) fn close_tab_preview(&mut self, cx: &mut Context<Self>) {
        self._tab_preview_task = None;
        if self.tab_card.close(cx) {
            cx.notify();
        }
    }

    /// 悬停到某个**非激活** tab:排一次 250ms 计时;到点后按 500ms 节拍续活。
    ///
    /// 卡还在屏幕上(开着,或刚离开上一个 tab、收起动画还没播完)时**不等**:直接换到
    /// 这个 tab,卡身从上一个 tab 下面滑过来,不重播开合(见 [`pane_preview::HoverCard`])。
    pub(super) fn schedule_tab_preview(&mut self, pane_id: String, cx: &mut Context<Self>) {
        self._tab_preview_task = None;
        if self.tab_card.is_warm() {
            if self.open_tab_preview(&pane_id, cx) {
                self._tab_preview_task = Some(cx.spawn(async move |this, cx| {
                    Self::keep_tab_preview_alive(this, cx).await;
                }));
            } else {
                self.close_tab_preview(cx);
            }
            return;
        }
        self._tab_preview_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(
                    pane_preview::HOVER_DELAY_MS,
                ))
                .await;
            let opened = this
                .update(cx, |this: &mut TerminalArea, cx| {
                    this.open_tab_preview(&pane_id, cx)
                })
                .unwrap_or(false);
            if opened {
                Self::keep_tab_preview_alive(this, cx).await;
            }
        }));
    }

    /// 弹出 / 换到 `pane_id` 的缩略图。核对:还悬着同一个 tab、且量到过矩形。
    pub(super) fn open_tab_preview(&mut self, pane_id: &str, cx: &mut Context<Self>) -> bool {
        if self.hovered_tab.as_deref() != Some(pane_id) {
            return false;
        }
        let Some(rect) = self.tab_rects.get(pane_id).copied() else {
            return false;
        };
        self.tab_card
            .open(pane_id.to_string(), rect, pane_preview::tab_anchor, cx);
        cx.notify();
        true
    }

    /// 卡开着期间按 500ms 节拍重画 —— 缩略图是活的(`MiniTerminalElement` 自己也按
    /// 这个节拍取数)。卡下了屏幕或所有窗口均不可见时自动停泵,避免后台空刷。
    pub(super) async fn keep_tab_preview_alive(
        this: gpui::WeakEntity<Self>,
        cx: &mut gpui::AsyncApp,
    ) {
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(mt_ui::MINI_REFRESH_MS))
                .await;
            let any_visible = cx.update(|cx| {
                cx.windows().iter().any(|w| {
                    w.update(cx, |_, window, _| window.is_visible())
                        .unwrap_or(false)
                })
            });
            let alive = this
                .update(cx, |this: &mut TerminalArea, cx| {
                    if !any_visible {
                        this.tab_card.close_now();
                        return false;
                    }
                    let warm = this.tab_card.is_warm();
                    if warm {
                        cx.notify();
                    }
                    warm
                })
                .unwrap_or(false);
            if !alive {
                return;
            }
        }
    }

    /// 组装 tab 缩略图浮层。
    ///
    /// 渲染处每帧重判(原版的双闸模式):tab 被关掉的那一帧就直接卸(不播动画,
    /// **状态本身也收掉** —— 用 ✕ 关掉被悬停的 tab 时点击被 `stop_propagation`
    /// 拦下,`on_hover(false)` 不一定来得及,旧锚点会残留到下次悬停);被激活了
    /// (画面已经在眼前)就播收起。
    ///
    /// 开合走 [`crate::popover_reveal::reveal_frame`]:卡里的内容(含
    /// `MiniTerminalElement`)按最终尺寸排,只被一个从 tab 下缘、以 tab 水平中心为原点
    /// 长出来的裁剪窗露出来 —— 不淡入,缩略图也不逐帧反解字号。
    pub(super) fn render_tab_preview(
        &mut self,
        layout: &SplitNode,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let mut frame = self.tab_card.frame(window)?;
        let (pane_id, rect) = (frame.key.clone(), frame.anchor);
        let (project_id, folded_away, still_hidden) = {
            let store = self.store.read(cx);
            // 卡上的名字与 tab 同口径,要项目 id 才查得到远程连接名
            let project_id = store.active_project_id.clone().unwrap_or_default();
            let leaf = layout.leaf_of_pane(&pane_id);
            // 最大化时**折叠掉的那些组整组都不在屏幕上**,连它们的「激活 tab」也该给
            // 预览 —— 展开态那条「只有非激活 tab 需要」的判据在这里不成立
            let folded_away = store
                .maximized_pane_id(&project_id)
                .and_then(|id| layout.leaf_of_pane(id))
                .is_some_and(|max_leaf| leaf.map(|l| l.id()) != Some(max_leaf.id()));
            let still_hidden = folded_away
                || match leaf {
                    Some(SplitNode::Leaf { active_pane_id, .. }) => active_pane_id != &pane_id,
                    _ => false,
                };
            (project_id, folded_away, still_hidden)
        };
        let Some(pane) = layout.pane(&pane_id) else {
            self.tab_card.close_now();
            return None;
        };
        if !still_hidden && self.tab_card.close(cx) {
            frame = self.tab_card.frame(window)?;
        }
        let store = self.store.read(cx);
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let info = pane_preview::snapshot_pane(pane, &project_id, 0, None, store, auto_resume, cx);
        let style = pane_preview::preview_style(store);
        let mut state = frame.reveal;
        state.origin_x = Some(rect.left() + rect.size.width / 2.0);
        // 底部折叠条或贴近窗口下缘被 `snap_to_window` 向上翻转的预览卡改为从底边往上长,
        // 紧贴触发 tab 上缘。纯展示层(`occlude: false`)退场期间由 `RevealFrame` 自动放行鼠标。
        let anchor_pos = pane_preview::tab_anchor(rect) + frame.offset;
        let flip_up = folded_away
            || f32::from(anchor_pos.y) + pane_preview::TAB_CARD_HEIGHT + 6.0
                > f32::from(window.viewport_size().height);
        let card = reveal_frame(
            pane_preview::tab_preview_card(&info, &style),
            state,
            Some(pane_preview::card_chrome()),
        )
        .anchor_bottom(flip_up);
        Some(
            deferred(
                anchored()
                    .position(anchor_pos)
                    // 「底下放不下就翻到 tab 上方」由贴边收拢代劳
                    .snap_to_window_with_margin(px(6.0))
                    .child(card),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
