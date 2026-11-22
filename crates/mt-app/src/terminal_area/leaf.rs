use super::*;

/// pane 整块跳过 render / prepaint / paint,直接重放上一帧(判据与三条前提见
/// `main.rs::cached_panel`)。此前每个可见 pane 每帧都要把整屏格子重新过一遍
/// 签名、重新出一遍图元 —— 分屏里一个 AI 在刷、另外三个闲着,就是四份的活。
///
/// # 逐条核过的「不 notify 就不会动」清单
///
/// 缓存只在四种情况下失效:本视图(或它的子视图)被 notify、bounds 变、
/// content_mask 变、`window.refresh()`。终端画面的每一种变化都落在其中之一:
///
/// - **输出**:reader 线程 → `redraw::request` → 节拍器 notify 的就是 `TerminalPane`;
/// - **键入 / IME 预编辑 / 粘贴 / 滚轮 / 拖选 / 停留复制 / 滚动条拖动与悬停**:
///   `TerminalView` 与终端元素的监听器里全是 `cx.notify(自己)`,沿视图路径标脏到本 pane;
/// - **滚动条淡出**:paint 里 `request_animation_frame`,下一帧 notify 当前视图;
/// - **查找**:查找条是本 pane 的子视图(改关键词 / 翻页即标脏);输出引起的重扫被
///   去抖挡下时,渲染层排一发延后 notify 兜底(`TerminalSearch::take_trailing_rescan`)。
///   ⚠️ 这类重扫发生在终端元素的 **prepaint**,而查找条的「n/总数」在同一帧的
///   **render** 就读过了 —— 结果一变,渲染层 `request_animation_frame` 再要一帧
///   (`FrameSync::repaint`)。缓存之前窗口任何一次重绘都会顺手重画查找条,这层
///   时序差被掩住了;套上缓存后漏了它,输出一停计数就停在扫描前的值;
/// - **焦点切换 / 窗口激活 / 缩放与 DPI**:gpui 走 `window.refresh()`,全窗缓存作废;
/// - **主题 / 字号 / 语言 / 减弱动效**:`set_theme` / `set_style` 自己 notify,
///   界面字号与语言走 `refresh_windows`,亮暗切换经 `Theme::change(window)` 刷新;
/// - **回滚行数 / 跳标记**:`set_scrollback` 与 `scroll_to_marker` 改了回看却可能
///   不触发任何 notify,两处已补上显式 notify;
/// - **pane 尺寸变化 / tab 推入推出动画**:bounds 变,缓存自然未命中。
///
/// 祖先不在动画里改 opacity(push 动画只动 `left`),`window_control_area` 也不在
/// pane 里,`main.rs::cached_panel` 的第二、三条前提同样满足。
///
/// 占位样式与 `TerminalPane::render` 根节点(两个分支都是 `size_full`)等价。
/// 占位节点没有子节点、min-content 为 0,**不会**把分栏撑宽 —— 分屏后三栏被
/// tab 栏撑出窗口那条是 `main.rs::columns_host` 缺 `min_w(0)`,套不套缓存实测
/// bounds 完全相同(见该函数注释)。
pub(super) fn cached_terminal(entity: Entity<crate::pane::TerminalPane>) -> AnyElement {
    entity
        .cached(gpui::StyleRefinement::default().size_full())
        .into_any_element()
}

/// 叶内切 tab 的方向性 push 两层(见文件头注释):新 pane 按方向推入、旧 pane
/// 同向推出;两层都是全尺寸 absolute,只动 `left`,PTY 不收 resize。没有在场
/// 记录时零包装 —— 只包终端主体,tab 栏不参与(它没换内容,动了反而怪)。
///
/// 关掉激活 tab / 把它拖去别的组时旧 pane 已不在本组,`exit_terminal` 为 `None`:
/// 只播进场层,新激活的 pane 照方向推入(与关面板同一口径)。
pub(super) fn leaf_terminal_layer(
    terminal: Option<Entity<crate::pane::TerminalPane>>,
    exit_terminal: Option<Entity<crate::pane::TerminalPane>>,
    tab_swap: Option<&(u64, f32, Option<String>)>,
) -> AnyElement {
    match terminal {
        Some(entity) => match tab_swap {
            Some((seq, dir, _old)) => {
                let (seq, dir) = (*seq, *dir);
                div()
                    .size_full()
                    .relative()
                    .overflow_hidden()
                    .children(exit_terminal.clone().map(|old_entity| {
                        div()
                            .absolute()
                            .top_0()
                            .w_full()
                            .h_full()
                            .child(old_entity)
                            // 正在推出的旧终端不接点击与悬停
                            .child(div().absolute().inset_0().occlude())
                            .with_animation(
                                gpui::SharedString::from(format!("tab-exit-{seq}")),
                                mt_ui::motion::TERMINAL_SWAP.animation(),
                                move |el, delta| el.left(gpui::relative(push_exit_x(dir, delta))),
                            )
                    }))
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .w_full()
                            .h_full()
                            .child(entity)
                            .with_animation(
                                gpui::SharedString::from(format!("tab-enter-{seq}")),
                                mt_ui::motion::TERMINAL_SWAP.animation(),
                                move |el, delta| el.left(gpui::relative(push_enter_x(dir, delta))),
                            ),
                    )
                    .into_any_element()
            }
            None => cached_terminal(entity),
        },
        None => div()
            .size_full()
            .bg(ui::bg_terminal())
            .flex()
            .items_center()
            .justify_center()
            .text_color(ui::text_muted())
            .child(t("paneGroup", "starting"))
            .into_any_element(),
    }
}

impl TerminalArea {
    pub(super) fn render_node(
        &mut self,
        node: &SplitNode,
        project_id: &str,
        available: Size<Pixels>,
        enter_axis: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match node {
            SplitNode::Leaf { id, .. } => {
                let leaf_id = id.clone();
                if self.suppress_leaf.as_deref() == Some(leaf_id.as_str()) {
                    // RevealBack 在飞:这一整组画在飞行层上,整树里只留空位。飞行层的
                    // 裁剪窗任何时刻都罩得住这块(从整幅收到它),空位不会露出来
                    return div().size_full().into_any_element();
                }
                let el = self.render_leaf(node, project_id, window, cx);
                if self.rendering_exit {
                    // 出场旧面板不走 pane_enter 与 fx::leaf_slot,避免污染活跃布局账本
                    return el;
                }
                let animate = self.animations_enabled(cx);
                let el =
                    self.wrap_pane_enter(&leaf_id, project_id, enter_axis, el, animate, window);
                // 关掉相邻分屏后,这一格从旧格子的裁剪窗长到新格子(见 [`fx`]);
                // 平时只记一下本帧的矩形,不裁剪、不挪
                fx::leaf_slot(el, self.fx.clone(), leaf_id).into_any_element()
            }
            SplitNode::Split {
                id,
                direction,
                children,
                sizes,
            } => {
                let state = self.split_state(id, cx);
                let horizontal = *direction == SplitDirection::Horizontal;
                let fractions = split_fractions(sizes, children.len());

                let panels: Vec<_> = children
                    .iter()
                    .enumerate()
                    .map(|(i, child)| {
                        let fraction = fractions.get(i).copied().unwrap_or(0.0) as f32;
                        // 子节点自己的可用尺寸:主轴按比例切,交叉轴照抄
                        let child_available = if horizontal {
                            Size {
                                width: available.width * fraction,
                                height: available.height,
                            }
                        } else {
                            Size {
                                width: available.width,
                                height: available.height * fraction,
                            }
                        };
                        let el = self.render_node(
                            child,
                            project_id,
                            child_available,
                            *direction,
                            window,
                            cx,
                        );
                        let main = if horizontal {
                            child_available.width
                        } else {
                            child_available.height
                        };
                        resizable_panel().size(main.max(px(1.0))).child(el)
                    })
                    .collect();

                let element_id = SharedString::from(format!("split-{id}"));
                let group = if horizontal {
                    h_resizable(element_id)
                } else {
                    v_resizable(element_id)
                };
                // 分隔条照内置样子画,外加「双击均分」的热区(见 [`split_handle`])
                let handles: gpui_base::ResizeHandleRenderer = {
                    let area = cx.weak_entity();
                    let state = state.clone();
                    let node_id = id.clone();
                    Rc::new(move |handle, _window, cx| {
                        let (area, state, node_id) = (area.clone(), state.clone(), node_id.clone());
                        Some(split_handle(
                            handle,
                            move |position, window, cx| {
                                let _ = area.update(cx, |area, cx| {
                                    area.equalize_split_at(&node_id, &state, position, window, cx)
                                });
                            },
                            cx,
                        ))
                    })
                };

                let store = self.store.clone();
                let node_id = id.clone();
                let pid = project_id.to_string();
                group
                    .with_state(&state)
                    .with_handle_appearance(handles)
                    .children(panels)
                    .on_resize(move |state, _window, cx| {
                        // ResizableState 给的是像素,布局树里存的是百分比(与磁盘
                        // 格式同口径),这里换算一次再写回。
                        let sizes: Vec<f64> = state
                            .read(cx)
                            .sizes()
                            .iter()
                            .map(|p| f32::from(*p) as f64)
                            .collect();
                        let Some(pct) = sizes_to_percent(&sizes) else {
                            return;
                        };
                        store.update(cx, |store, cx| {
                            store.set_split_sizes(&pid, &node_id, pct, cx)
                        });
                    })
                    .into_any_element()
            }
        }
    }

    /// 叶子的进场动画(对应原版 `styles.css` 的 `.pane-enter`,0.26s)。
    /// 形态改成与换场滑入同款的**纯滑入**(见文件头注释)—— 原版的
    /// `opacity 0→1 + scale(0.97)` 两截都不用了:淡入在深色终端上读作
    /// 「闪一下」(用户否掉);缩放在 gpui 没有 transform,只能改尺寸去凑,
    /// 而那会让 `TerminalView` 按小一号的格子数一路 resize 到 PTY
    /// (启动时每个叶子都来一遍)。滑入的 absolute 层尺寸恒定,两条都躲开。
    ///
    /// # 三条照抄来的语义
    ///
    /// 1. **每个叶子只播一次**:第一次渲染到这个 `(项目, 叶子)` 时起表,
    ///    之后拿同一条进度 —— 等价于 React 里「这层 DOM 只挂载一次」;
    /// 2. **切项目不重播**(原版是 `display:none` 不卸载),所以键里带项目 id
    ///    且不按帧回收;
    /// 3. **不过减弱动效的闸**:`.pane-enter` 在原版 reduce 段里被**点名豁免**
    ///    (`styles.css:441-443`),开了「减弱动态效果」照样播。
    pub(super) fn wrap_pane_enter(
        &mut self,
        leaf_id: &str,
        project_id: &str,
        axis: SplitDirection,
        el: AnyElement,
        animate: bool,
        window: &Window,
    ) -> AnyElement {
        let key = format!("{project_id}\u{1}{leaf_id}");
        let progress = self
            .pane_enter
            .entry(key)
            .or_insert_with(|| mt_ui::motion::Transition::new(mt_ui::motion::PANE_ENTER))
            .drive(window);
        // 「启用动画」关着时进度照走(静默跑完),中途打开不补播
        if progress >= 1.0 || !animate {
            // 跑完就把包装层整个摘掉:少两层空 div,也少一次裁剪。
            // (匿名 div 不带 ElementId,加/摘不影响子树的元素状态路径)
            return el;
        }
        // 滑入方向与分屏轴一致:左右分屏从右侧滑入,上下分屏从下方滑入,全程不透明
        let shift = px(swap_slide_offset(progress));
        let inner = match axis {
            SplitDirection::Horizontal => div().absolute().top_0().left(shift).w_full().h_full(),
            SplitDirection::Vertical => div().absolute().left_0().top(shift).w_full().h_full(),
        };
        div()
            .size_full()
            .relative()
            .overflow_hidden()
            .child(inner.child(el))
            .into_any_element()
    }

    /// 最大化视图 = 铺满的那一格 + 其余叶子的折叠标题条(码在底部)。
    ///
    /// v0.14.0 的最大化是「整树其余 pane 直接不进元素树」,别的终端连状态灯都
    /// 看不见 —— 另一格里的 AI 跑完了毫无提示,只能先还原再找。折叠条把
    /// 「看得见 + 点得回去」补回来,代价只有每格 26px。
    ///
    /// **折叠条不按原方位摆**(左右分屏的那格也码到底部):26px 宽的竖条上放不下
    /// 横排文字,只剩一颗状态灯的话还不如统一成整宽横条 —— 标题、品牌图标、
    /// 未读标都能完整显示。原布局树一个字没动,还原后照旧。
    pub(super) fn render_maximized(
        &mut self,
        layout: &SplitNode,
        leaf: &SplitNode,
        project_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let body = self.render_leaf(leaf, project_id, window, cx);
        let leaf_id = leaf.id().to_string();
        let others: Vec<&SplitNode> = layout
            .leaves()
            .into_iter()
            .filter(|l| l.id() != leaf_id)
            .collect();
        let bars: Vec<AnyElement> = others
            .into_iter()
            .map(|l| self.render_collapsed_leaf(l, project_id, cx))
            .collect();
        // 折叠区自己滚,铺满那格恒占剩下的全部(`min_h(0)` 是 flex 子项能收缩的
        // 前提 —— 缺了它内容撑高时会把折叠区顶出可视区)。实际高度 = min(条数 × 条高,
        // 上限),还原飞行层按同一公式占位([`collapsed_zone_height`])
        let zone_max = px(collapsed_zone_max(f32::from(self.area_size.height)));
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(div().flex_1().min_h(px(0.0)).child(body))
            .child(
                div()
                    .id("collapsed-zone")
                    .flex()
                    .flex_col()
                    .flex_none()
                    .max_h(zone_max)
                    .overflow_y_scroll()
                    .children(bars),
            )
            .into_any_element()
    }

    /// 一条折叠标题条:该叶子的每个 tab 一颗状态灯 + 品牌图标 + 标题 + 未看绿点。
    ///
    /// 刻意**不画**新建 / 查找 / 分屏 / 关组那套控件簇 —— 折叠条是导航件不是
    /// 工作区,按钮挤在 26px 里全是误点。关闭 / 重命名仍走 tab 右键菜单(与展开态
    /// 同一个 [`tab_menu`]),悬停缩略图也照给。
    pub(super) fn render_collapsed_leaf(
        &mut self,
        node: &SplitNode,
        project_id: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let SplitNode::Leaf {
            id: leaf_id,
            panes,
            active_pane_id,
        } = node
        else {
            return div().into_any_element();
        };
        let store = self.store.read(cx);
        let Some(active_id) = panes
            .iter()
            .find(|p| &p.id == active_pane_id)
            .or_else(|| panes.first())
            .map(|p| p.id.clone())
        else {
            return div().into_any_element();
        };
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let pid = project_id.to_string();
        let leaf = leaf_id.clone();
        // 显示数据先摘成 owned:下面每个 tab 都要往 move 闭包里搬,借着 store 走不了。
        // 字段口径与展开态 tab 逐字一致(见 render_leaf_tab_bar)
        struct CollapsedTab {
            pane_id: String,
            primary: String,
            secondary: Option<String>,
            shell_name: String,
            light: StatusLight,
            ssh_connected: Option<String>,
            vendor: Option<AiVendor>,
            unseen: bool,
        }
        let tabs: Vec<CollapsedTab> = panes
            .iter()
            .map(|pane| {
                // 品牌图标的取值口径与展开态 tab 逐字一致(见 render_leaf 的 vendors)
                let vendor = pane
                    .shows_ai_session(auto_resume)
                    .then(|| pane.ai_agent())
                    .flatten()
                    .and_then(|agent| {
                        AiVendor::from_session_type(agent)
                            .or_else(|| AiVendor::infer(Some(agent), None))
                    });
                let (primary, secondary) = store.pane_title_parts(&pid, pane);
                CollapsedTab {
                    pane_id: pane.id.clone(),
                    primary,
                    secondary,
                    shell_name: pane.shell_name.clone(),
                    light: pane.light(),
                    ssh_connected: pane.ssh_connected.clone(),
                    vendor,
                    unseen: store.is_pane_unseen_done(&pane.id),
                }
            })
            .collect();

        // 焦点句柄与展开态共用 `tab_focus` 那张表(它按整棵树保留,见 render 里的
        // retain):折叠 ↔ 展开来回切时 Tab 焦点不丢
        for tab in &tabs {
            if !self.tab_focus.contains_key(&tab.pane_id) {
                let handle = cx.focus_handle();
                self.tab_focus.insert(tab.pane_id.clone(), handle);
            }
        }

        let mut bar = div()
            .id(SharedString::from(format!("collapsed-{leaf}")))
            .flex()
            .items_center()
            .flex_none()
            .h(px(TAB_BAR_H))
            .w_full()
            .overflow_hidden()
            .bg(ui::bg_elevated())
            .border_t_1()
            .border_color(ui::border_subtle())
            .text_size(ui::font_px(12.0))
            .cursor_pointer()
            .hover(|el| el.bg(ui::bg_overlay()))
            // 条上任意空白处点一下 = 这一组接管铺满(不必瞄准 tab)
            .on_click(cx.listener({
                let (pid, anchor) = (pid.clone(), active_id.clone());
                move |this: &mut TerminalArea, _event: &ClickEvent, window, cx| {
                    this.take_over_maximized(&pid, &anchor, window, cx);
                }
            }));

        let this_area = cx.entity();
        for CollapsedTab {
            pane_id,
            primary,
            secondary,
            shell_name,
            light,
            ssh_connected,
            vendor,
            unseen,
        } in tabs
        {
            let is_active = pane_id == active_id;
            let focus = self.tab_focus.get(&pane_id).cloned();
            let (pid_click, pane_click) = (pid.clone(), pane_id.clone());
            let (pid_key, pane_key) = (pid.clone(), pane_id.clone());
            // 右键菜单的「重命名」只预填主段(理由见展开态 tab 的同一处注释)
            let (pid_menu, pane_menu, label_menu) = (pid.clone(), pane_id.clone(), primary.clone());
            let pane_hover = pane_id.clone();
            let pane_rect = pane_id.clone();
            let this_rect = this_area.clone();
            bar = bar.child(
                div()
                    .id(SharedString::from(format!("collapsed-tab-{pane_id}")))
                    .relative()
                    .flex()
                    .items_center()
                    .h_full()
                    .gap(px(6.0))
                    .px(px(10.0))
                    .flex_none()
                    .when_some(focus, |el, focus| el.track_focus(&focus).tab_index(0))
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                        if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                            cx.stop_propagation();
                            this.take_over_maximized(&pid_key, &pane_key, window, cx);
                        }
                    }))
                    // 悬停缩略图:折叠条上**每个** tab 都值得预览 —— 这一整组的画面
                    // 一个都不在屏幕上(展开态是「只有非激活 tab 需要」,判据在
                    // `render_tab_preview` 里按最大化态放宽)
                    .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                        let mine = this.hovered_tab.as_deref() == Some(pane_hover.as_str());
                        if *hovered {
                            if mine {
                                return;
                            }
                            this.hovered_tab = Some(pane_hover.clone());
                            this.schedule_tab_preview(pane_hover.clone(), cx);
                        } else {
                            if !mine {
                                return;
                            }
                            this.hovered_tab = None;
                            this.close_tab_preview(cx);
                        }
                        cx.notify();
                    }))
                    // 缩略图的锚点(与展开态 tab 共用 `tab_rects`:两者互斥,
                    // 同一个 pane 不会在一帧里既折叠又展开)。故意不 notify
                    .child({
                        canvas(
                            move |bounds, _window, cx| {
                                this_rect.update(cx, |area: &mut TerminalArea, _cx| {
                                    area.tab_rects.insert(pane_rect.clone(), bounds);
                                });
                            },
                            |_, _, _, _| {},
                        )
                        // 钉住 inset:tab 是带 px(10) 的 flex,不写会按静态位置右移 10px,
                        // 缩略图锚点跟着偏(见 `segment_slider::IndicatorSlider::probe`)
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                    })
                    .text_color(if is_active {
                        ui::text_primary()
                    } else {
                        ui::text_muted()
                    })
                    .on_click(cx.listener(move |this, _event: &ClickEvent, window, cx| {
                        cx.stop_propagation();
                        this.take_over_maximized(&pid_click, &pane_click, window, cx);
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.close_tab_preview(cx);
                            let entries =
                                tab_menu(&this.store, &pid_menu, &pane_menu, &label_menu, cx);
                            menu::show(event.position, entries, window, cx);
                        }),
                    )
                    // 折叠条底色是 bg_elevated(悬停 bg_overlay),挖空笔画取前者
                    .child(tab_icon_slot(
                        &pane_id,
                        light,
                        ssh_connected.as_deref(),
                        &shell_name,
                        is_active,
                        ui::bg_elevated(),
                    ))
                    .when_some(vendor, |el, vendor| {
                        // VectorIcon 不继承 text_color,跟着 tab 的明暗自己喂
                        let color = if is_active {
                            ui::text_primary()
                        } else {
                            ui::text_muted()
                        };
                        el.child(BrandIcon::new(Some(vendor)).size(px(12.0)).color(color))
                    })
                    // 与展开态同一套两段式标题;折叠条上的 tab 没有 max_w,
                    // 副段再长也别把整条撑爆
                    .child(
                        div()
                            .min_w(px(0.0))
                            .max_w(px(TAB_CHIP_MAX_W))
                            .flex()
                            .items_center()
                            .child(tab_label(
                                primary.into(),
                                secondary.map(SharedString::from),
                                is_active,
                            )),
                    )
                    // 「做完了还没看」的绿点:绝对定位在 tab 自己的右内衬里,来去都是
                    // 零宽度变化(此前插在标题后面,每次完成都把后面的 tab 整排推开)
                    .when(unseen, |el| {
                        el.child(
                            div()
                                .absolute()
                                .right(px(COLLAPSED_UNSEEN_INSET))
                                .top(px((TAB_BAR_H - TAB_UNSEEN_DOT) / 2.0))
                                .w(px(TAB_UNSEEN_DOT))
                                .h(px(TAB_UNSEEN_DOT))
                                .rounded_full()
                                .bg(ui::color_success()),
                        )
                    }),
            );
        }

        // 右端那颗「点这里铺满」的提示图标。**不挂自己的点击** —— 整条都是热区,
        // 它只是把可点性说出来,顺便当那句 tooltip 的锚点。
        //
        // tooltip 刻意**不挂在整条上**:挂上去的话鼠标停在 tab 上会与悬停缩略图
        // 两个浮层一起弹(tooltip 认的是父元素的 hitbox,子元素挡不住)。
        bar.child(
            div()
                .id(SharedString::from(format!("collapsed-hint-{leaf}")))
                .ml_auto()
                .flex()
                .items_center()
                .h_full()
                .px(px(CTRL_CLUSTER_PAD))
                .opacity(0.5)
                .hover(|el| el.opacity(1.0))
                .tip(t("paneGroup", "collapsedHint"))
                .child(VectorIcon::new(ICON_MAXIMIZE, px(CTRL_ICON)).ink(ui::text_muted())),
        )
        .into_any_element()
    }

    /// 动画总开关(设置页「启用动画」,缺省开启)。2026-11-21 起管**全部**动画,
    /// 真身是 [`mt_ui::motion::animations_enabled`](与配置的同步见 `crate::motion::sync_switch`)。
    /// 终端区这批换场在**检测层**就判它(关掉时不记过渡、不起计时器),比让过渡
    /// 直达终态更省 —— 出场层一帧都不画。
    /// 远程断线覆盖层:**保留 pane**,点一下在同一 pane 重连。
    pub(super) fn render_leaf_reconnect_overlay(
        &mut self,
        project_id: &str,
        pane_id: &str,
        leaf_id: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (pid, pane_id) = (project_id.to_string(), pane_id.to_string());
        div()
            .id(gpui::SharedString::from(format!("reconnect-{leaf_id}")))
            .absolute()
            .inset_0()
            // 遮罩自己吃点击:底下终端的聚焦监听不该抢走这一下
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            // 原版 `bg-black/55 backdrop-blur-[1px]`;gpui 没有
            // backdrop-filter,只留半透明黑
            .bg(gpui::rgba(0x0000008c))
            .child(
                div()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_secondary())
                    .child(t("paneGroup", "remoteDisconnected")),
            )
            .child(
                ui::ghost_button(
                    gpui::SharedString::from(format!("reconnect-btn-{leaf_id}")),
                    t("paneGroup", "reconnect"),
                )
                .on_click(cx.listener(
                    move |this: &mut TerminalArea, _event, window, cx| {
                        let (pid, pane_id) = (pid.clone(), pane_id.clone());
                        this.store.update(cx, |store, cx| {
                            // 一步含:kill 旧 PTY / 清标记与退出登记 /
                            // 起新 PTY / 回写 pane(见 store 那边的注释)
                            store.reset_pane_for_reconnect(&pid, &pane_id, cx);
                            store.focus_pane(&pid, &pane_id, window, cx);
                        });
                    },
                )),
            )
    }

    pub(super) fn render_leaf(
        &mut self,
        node: &SplitNode,
        project_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let SplitNode::Leaf {
            id: leaf_id,
            panes,
            active_pane_id,
        } = node
        else {
            return div().into_any_element();
        };

        let active = panes
            .iter()
            .find(|p| &p.id == active_pane_id)
            .or_else(|| panes.first());
        let Some(active) = active else {
            return div().into_any_element();
        };

        // ── 检测叶内切 tab,起一条方向性 push 过渡(出场旧面板不检测也不写记录)────
        let tab_swap = if self.rendering_exit {
            None
        } else {
            self.note_leaf_tab_swap(leaf_id, panes, &active.id, cx)
        };

        let store = self.store.read(cx);
        let terminal = active.pty_id.and_then(|id| store.terminal(id)).cloned();
        // 出场层的旧终端实体(旧 pane 还没起 PTY 时出场层只好省略;旧 pane 已不在本组 ——
        // 关掉了 / 拖去别的组 —— 时过渡记录里压根没有它,只播进场)
        let exit_terminal = tab_swap.as_ref().and_then(|(_, _, old)| {
            store
                .project_state(project_id)
                .and_then(|s| s.pane(old.as_deref()?))
                .and_then(|p| p.pty_id)
                .and_then(|id| store.terminal(id))
                .cloned()
        });
        // 远程 pane 的断线覆盖层(`PaneGroup.tsx:329-333` 的 `showReconnect`):
        // 判据是「项目是 SSH 远程项目 **且** 这条 PTY 已登记退出」。
        // ⚠️ **本地 pane 不进这条路**(原版 `remote &&` 那一半就是这个闸)—— 本地退出
        // 走 pane 自己底部那条「重新启动 / 关闭」(`pane.rs::render_exit_bar`):不遮
        // 回滚,退出前的输出照样能回看。整屏遮罩只留给远程断线。
        // 断链(连接被删)照样算远程项目,遮罩照出:重连会再失败一次并把明确的
        // 断链错误画进 pane,比静默什么都不发生强。
        let show_reconnect = store.is_remote_project(project_id)
            && active
                .pty_id
                .map(|id| store.is_pty_exited(id))
                .unwrap_or(false);

        let active_id = active.id.clone();
        let pid = project_id.to_string();
        let leaf = leaf_id.clone();
        // 拖拽相关的三份视图状态一律与它与门(见字段注释)
        let dragging = cx.has_active_drag();

        // tab 栏 = 本体 + 右侧控件簇,外面再包一层落点层(理由见那个方法)
        let mut bar = self.render_leaf_tab_bar(panes, active, project_id, leaf_id, dragging, cx);
        bar = bar.child(self.render_leaf_controls(panes, active, project_id, leaf_id, cx));
        // 双击 tab 栏**空白处**最大化 / 还原。原版靠 `e.target.closest('[data-pane-tab],button')`
        // 排除 tab 与按钮;gpui 侧改由那些子元素自己 `stop_propagation`(见各处注释)
        // —— 效果相同,而且不需要在这里维护一张「哪些子元素算控件」的名单。
        let pid_dblclick = pid.clone();
        let anchor_dblclick = active_id.clone();
        bar = bar.on_click(cx.listener(move |this, event: &ClickEvent, _window, cx| {
            if click_count(event) < 2 {
                return;
            }
            let (pid, anchor) = (pid_dblclick.clone(), anchor_dblclick.clone());
            this.store.update(cx, |store, cx| {
                store.toggle_maximized_leaf(&pid, &anchor, cx)
            });
        }));
        let bar_layer = self.render_leaf_drop_layer(bar, project_id, leaf_id, dragging, cx);

        let pid_focus = pid.clone();
        let active_for_focus = active_id.clone();
        let pid_drop = pid.clone();
        let drop_pane_id = active_id.clone();
        // 拖拽中断(松手在窗外)后 gpui 会清 active_drag 并重画,与它与门就不必
        // 到处补清理 —— 与 `project_list.rs` 里那份高亮同一套判据。
        let file_drop_over =
            cx.has_active_drag() && self.file_drop_pane.as_deref() == Some(active_id.as_str());
        // 方向导航要知道每个 pane 画在哪 —— canvas 只量不画,量完存进本视图。
        // 这里**故意不 notify**:量尺寸再触发重画就是每帧一个死循环。
        let this = cx.entity();
        let rect_pane_id = active_id.clone();
        // 关分屏后的扩张中,整格被 LeafSlot 挪着画:量尺减掉那份偏移,记的仍是排版位置
        let rect_fx = self.fx.clone();

        // 终端区落点预览的档位(只画本组自己那一份)
        let body_zone = self
            .pane_drop
            .as_ref()
            .filter(|(id, _)| dragging && id == &leaf)
            .map(|(_, zone)| *zone);
        let leaf_for_body = leaf.clone();
        let leaf_for_body_drop = leaf.clone();
        let pid_pane_drop = pid.clone();
        let anchor_pane_drop = active_id.clone();

        // ⚠️ 不刷底色:终端区着色**只保留 TerminalArea 根容器一层**(见 render 尾部
        // 那句 `.bg(ui::bg_terminal())`)。原版同款口径 —— `styles.css:151` 与
        // `themePackManager.ts:294`「着色统一由 --bg-terminal 容器层承担,避免
        // 容器/wrapper/xterm 三层叠加」:背景图主题下 bg_terminal 是半透明 rgba,
        // 这里再刷一层就是透明度叠乘,图被盖死(原版 PaneGroup 根也没有背景)。
        //
        // 也**不画边框**:原版 PaneGroup 根没有任何 border(`PaneGroup.tsx:393`),
        // 焦点表达靠激活 tab 的 accent 顶线 + 光标实心/空心两处;此前的
        // group_focused accent 描边是 GPUI 侧自加的,真机上整圈橙线喧宾夺主
        // (用户报障),按原版口径删除。
        // 远程断线覆盖层(判据见上面的 `show_reconnect`)
        let reconnect = show_reconnect
            .then(|| self.render_leaf_reconnect_overlay(project_id, &active_id, leaf_id, cx));

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(bar_layer)
            .child(
                div()
                    .id(gpui::SharedString::from(format!("pane-body-{leaf}")))
                    .flex_1()
                    .relative()
                    .overflow_hidden()
                    // ── pane 拖到终端区:四边分屏 / 中央并组 ────────────
                    .on_drag_move(cx.listener({
                        let leaf_id = leaf_for_body.clone();
                        move |this: &mut TerminalArea,
                              event: &gpui::DragMoveEvent<crate::dnd::DragPane>,
                              _window,
                              cx| {
                            let dragged = event.drag(cx).clone();
                            this.note_pane_drag_over(
                                &leaf_id,
                                &dragged,
                                event.bounds,
                                event.event.position,
                                cx,
                            );
                        }
                    }))
                    .on_drop(cx.listener({
                        let (pid, leaf_id, anchor) = (
                            pid_pane_drop.clone(),
                            leaf_for_body_drop.clone(),
                            anchor_pane_drop.clone(),
                        );
                        move |this: &mut TerminalArea, item: &crate::dnd::DragPane, window, cx| {
                            this.drop_on_pane_body(&pid, &leaf_id, &anchor, item, window, cx);
                        }
                    }))
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        this.store.update(cx, |store, cx| {
                            store.focus_pane(&pid_focus, &active_for_focus, window, cx)
                        });
                    }))
                    // ── 拖文件进终端(改造清单 #8 链路③)────────────────
                    //
                    // 两个来源共用这一处落点:文件树的 `DragFilePath` 与资源管理器的
                    // `ExternalPaths`(gpui 把系统 FileDrop 翻译成内部 drag,见 `dnd` 模块)。
                    // 写入走 `AppStore::write_to_pane` —— 它刻意经 `TerminalPane::write`,
                    // 好让 AI 输入检测那条链路看得见这段文本,**不许改成裸 PTY 写**。
                    .on_drag_move(cx.listener({
                        let pane_id = drop_pane_id.clone();
                        move |this: &mut TerminalArea,
                              event: &gpui::DragMoveEvent<crate::dnd::DragFilePath>,
                              _window,
                              cx| {
                            this.note_file_drag_over(
                                &pane_id,
                                event.bounds,
                                event.event.position,
                                cx,
                            );
                        }
                    }))
                    .on_drag_move(cx.listener({
                        let pane_id = drop_pane_id.clone();
                        move |this: &mut TerminalArea,
                              event: &gpui::DragMoveEvent<gpui::ExternalPaths>,
                              _window,
                              cx| {
                            this.note_file_drag_over(
                                &pane_id,
                                event.bounds,
                                event.event.position,
                                cx,
                            );
                        }
                    }))
                    .on_drop(cx.listener({
                        let (pid, pane_id) = (pid_drop.clone(), drop_pane_id.clone());
                        move |this: &mut TerminalArea,
                              item: &crate::dnd::DragFilePath,
                              window,
                              cx| {
                            let text = crate::dnd::quote_path(&item.path);
                            this.insert_path_into_pane(&pid, &pane_id, &text, window, cx);
                        }
                    }))
                    .on_drop(cx.listener({
                        let (pid, pane_id) = (pid_drop.clone(), drop_pane_id.clone());
                        move |this: &mut TerminalArea, item: &gpui::ExternalPaths, window, cx| {
                            let text = crate::dnd::quote_paths(item.paths());
                            this.insert_path_into_pane(&pid, &pane_id, &text, window, cx);
                        }
                    }))
                    .when(!self.rendering_exit, |el| {
                        el.child(
                            canvas(
                                move |bounds: Bounds<Pixels>, _window, cx| {
                                    let bounds = rect_fx.borrow().unshift(bounds);
                                    this.update(cx, |area: &mut TerminalArea, _cx| {
                                        area.pane_rects.insert(
                                            rect_pane_id.clone(),
                                            PaneRect {
                                                pane_id: rect_pane_id.clone(),
                                                left: bounds.origin.x.into(),
                                                top: bounds.origin.y.into(),
                                                width: bounds.size.width.into(),
                                                height: bounds.size.height.into(),
                                            },
                                        );
                                    });
                                },
                                |_, _, _, _| {},
                            )
                            .absolute()
                            .size_full(),
                        )
                    })
                    .child(leaf_terminal_layer(
                        terminal,
                        exit_terminal,
                        tab_swap.as_ref(),
                    ))
                    // 「释放以插入路径」的虚线框。与 `cx.has_active_drag()` 与门:
                    // 拖拽被中断时 gpui 会清 active_drag 并重画,残留状态自动失效。
                    .when(file_drop_over, |el| el.child(drop_hint()))
                    // pane 落点预览:center 铺满 = 并入本组,半屏 = 往那个方向分屏。
                    // **落下没有动作的场景压根不会有档位**(判档时就滤掉了,
                    // 见 `note_pane_drag_over`)—— 原版三轮评审的最终口径
                    .children(body_zone.map(zone_overlay))
                    // 远程断线覆盖层:**保留 pane**,点一下在同一 pane 重连
                    .children(reconnect),
            )
            .into_any_element()
    }
}
