use super::*;

/// `paint_mouse_listeners` 拆出的共用帧几何上下文,避免五个监听器各拷一遍字段。
struct MouseBindCtx {
    view_id: EntityId,
    hitbox: Hitbox,
    origin: Point<Pixels>,
    cell_size: Size<Pixels>,
    columns: usize,
    screen_lines: usize,
    state: TerminalElementState,
    mode: TermMode,
    alt_screen: bool,
    bar: Option<ScrollbarLayout>,
    element_size: (f32, f32),
}

impl TerminalElement {
    /// # 为什么这一整套监听器里一个 `window.refresh()` 都没有
    ///
    /// `refresh()` 置的是 `Window::refreshing` 位(gpui-pre `window.rs:2248-2253`),
    /// 而 view 级缓存的复用判据里就有 `&& !window.refreshing`
    /// (`view.rs:386-392`)—— 一置位,**全窗所有 `.cached(style)` 的 view 当帧全部作废**。
    /// 宿主那边 `main.rs::cached_panel` 套着项目列表 / 文件树 / 面板竖条三块,
    /// 于是「拖选一次 = 每个 mouse-move 把那三块整套重跑一遍 render」。
    ///
    /// 换成 `cx.notify(view_id)`:`App::notify` 走 `invalidate_view`
    /// (`window.rs:166-191`)把窗口标脏 + 唤醒平台帧源,绘制时
    /// `mark_view_dirty`(`window.rs:2140-2152`)只沿**这个 view 到根**那条路径
    /// 标脏,旁支的 cached 面板照走缓存。语义与上游 `request_animation_frame`
    /// (`window.rs:2606-2609`,同样是 `cx.notify(current_view())`)一致 ——
    /// 滚动条淡出本来就走那条路,这里只是把鼠标交互也并过去。
    /// 本模块自己量过这个倍率:见 `crate::motion` 的「refresh 路单帧 CPU
    /// ≈ notify 路 4.6 倍」。
    ///
    /// 这些回调改的**全是终端元素自己的画面**(回看位置、选区、滚动条滑块),
    /// 没有一处会改别的面板要读的状态,所以逐处核对下来一个都不必保留 refresh。
    /// 唯一的例外是宿主主动关心的「停留复制」——那条另有 `on_selection_copied`
    /// 回调让宿主自己 notify(`pane.rs` 里弹「已复制」气泡的那段)。
    pub(super) fn paint_mouse_listeners(
        &self,
        prepared: &PreparedFrame,
        window: &mut Window,
        _cx: &mut App,
    ) {
        // paint 阶段取当前视图 id(`current_view` 断言必须在 prepaint/paint 里),
        // move 进下面每个闭包 —— 闭包跑的时候已经不在绘制期,取不到了
        let view_id = window.current_view();
        let hitbox = prepared.hitbox.clone();
        let origin = prepared.origin;
        let cell_size = prepared.cell_size;
        let columns = prepared.columns;
        let screen_lines = prepared.screen_lines;
        let state = prepared.state.clone();
        let mode = prepared.mode;
        let alt_screen = mode.contains(TermMode::ALT_SCREEN);
        let bar = prepared.scrollbar;
        let element_size = (
            f32::from(prepared.hitbox.bounds.size.width),
            f32::from(prepared.hitbox.bounds.size.height),
        );

        // ── 拖选自动滚屏的几何每帧刷新:指针停在区外不动时节拍任务读的就是这一份
        //    (拖着的时候窗口被缩放、分屏比例被改也跟得上)。一次 Cell 读写,不分配
        {
            let bounds = prepared.hitbox.bounds;
            let mut shared = state.autoscroll.get();
            shared.origin = origin;
            shared.cell_size = cell_size;
            shared.columns = columns;
            shared.screen_lines = screen_lines;
            shared.zone = ScrollZone::new(
                f(bounds.top()),
                f(bounds.bottom()),
                f(window.viewport_size().height),
            );
            state.autoscroll.set(shared);
        }
        let scroll_ctx = AutoScrollCtx {
            emulator: self.emulator.clone(),
            selecting: state.selecting.clone(),
            drag_anchor: state.drag_anchor.clone(),
            shared: state.autoscroll.clone(),
            touched: state.scrollbar_touched.clone(),
            dwell: state.dwell.clone(),
            dwell_task: state.dwell_task.clone(),
            dwell_cfg: self.dwell,
            on_copied: self.on_selection_copied.clone(),
            element_size,
            view_id,
        };

        let bind = MouseBindCtx {
            view_id,
            hitbox,
            origin,
            cell_size,
            columns,
            screen_lines,
            state,
            mode,
            alt_screen,
            bar,
            element_size,
        };

        self.bind_wheel_listener(&bind, window);
        self.bind_mouse_down_listener(&bind, window);
        self.bind_mouse_move_listener(&bind, scroll_ctx, window);
        self.bind_mouse_up_listener(&bind, window);
        self.bind_mouse_exit_listener(&bind, window);
    }

    fn bind_wheel_listener(&self, bind: &MouseBindCtx, window: &mut Window) {
        let MouseBindCtx {
            view_id,
            origin,
            cell_size,
            columns,
            screen_lines,
            mode,
            alt_screen,
            ..
        } = *bind;
        let hitbox = &bind.hitbox;
        let state = &bind.state;
        // ── 滚轮
        //
        //  优先级:Ctrl+滚轮缩放 > 鼠标上报 > alt screen 方向键 > 本地回看。
        //  上报优先是因为开了上报的 TUI(htop / lazygit / fzf)自己有滚动语义,
        //  我们代劳只会让它收到一堆无意义的方向键。Ctrl+滚轮排在最前,与 Windows
        //  Terminal / VS Code 终端同一手感:不管里面跑着什么,按住 Ctrl 滚就是缩放。
        {
            let emulator = self.emulator.clone();
            let hitbox = hitbox.clone();
            let remainder = state.scroll_remainder.clone();
            let on_input = self.on_input.clone();
            let on_zoom = self.on_zoom.clone();
            let touched = state.scrollbar_touched.clone();
            let app_cursor = mode.contains(TermMode::APP_CURSOR);
            let link_hint = state.link_hint.clone();
            let link_hint_task = state.link_hint_task.clone();
            window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.should_handle_scroll(window) {
                    return;
                }
                let lines = match event.delta {
                    ScrollDelta::Lines(p) => p.y,
                    ScrollDelta::Pixels(p) => f(p.y) / f(cell_size.height).max(1.0),
                };
                let total = remainder.get() + lines;
                let whole = total.trunc();
                remainder.set(total - whole);
                if whole == 0.0 {
                    return;
                }
                // 滚一下等于移开:鼠标底下换了文字,悬停提示要重新停留才出
                link_hint_task.borrow_mut().take();
                if link_hint.borrow_mut().cancel() {
                    cx.notify(view_id);
                }
                // 一次滚轮事件只走一档(一格滚轮常报 3 行,按行数走会一下跳三档);
                // 触控板的像素增量照样先攒够一行才算一档
                if event.modifiers.control
                    && let Some(on_zoom) = on_zoom.as_ref()
                {
                    on_zoom(if whole > 0.0 { 1 } else { -1 }, window, cx);
                    return;
                }
                let mods = modifiers_of(&event.modifiers);

                if !prefers_local_handling(mode, mods) {
                    let Some(on_input) = on_input.as_ref() else {
                        return;
                    };
                    let (col, row, _) =
                        Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);
                    let dir = if whole > 0.0 {
                        WheelDir::Up
                    } else {
                        WheelDir::Down
                    };
                    let mut payload = Vec::new();
                    for _ in 0..whole.abs() as usize {
                        if let Some(bytes) = mouse_report_bytes(
                            mode,
                            MouseAction::Wheel(dir),
                            mods,
                            GridPos::new(col, row),
                        ) {
                            payload.extend_from_slice(&bytes);
                        }
                    }
                    if !payload.is_empty() {
                        on_input(&payload, window, cx);
                    }
                    return;
                }

                if alt_screen {
                    // alt screen(vim / less 这类全屏程序)没有回看缓冲,
                    // 改成等价地敲方向键 —— 这也是 xterm 一贯的做法。
                    let Some(on_input) = on_input.as_ref() else {
                        return;
                    };
                    let payload = alt_screen_scroll_bytes(whole as i32, app_cursor);
                    on_input(&payload, window, cx);
                    return;
                }

                emulator.with_term_mut(|term| term.scroll_display(Scroll::Delta(whole as i32)));
                // 滚了就把滚动条亮起来(淡出重新计时)
                touched.set(Some(Instant::now()));
                // 变的只有本元素的回看位置与滚动条,不必用 refresh 把别的面板一起作废
                cx.notify(view_id);
            });
        }
    }

    fn bind_mouse_down_listener(&self, bind: &MouseBindCtx, window: &mut Window) {
        let MouseBindCtx {
            view_id,
            origin,
            cell_size,
            columns,
            screen_lines,
            mode,
            bar,
            element_size,
            ..
        } = *bind;
        let hitbox = &bind.hitbox;
        let state = &bind.state;
        // ── 按下:上报,或开本地选择
        {
            let emulator = self.emulator.clone();
            let hitbox = hitbox.clone();
            let selecting = state.selecting.clone();
            let drag_anchor = state.drag_anchor.clone();
            let reported = state.reported_button.clone();
            let last_cell = state.last_reported_cell.clone();
            let on_input = self.on_input.clone();
            let bar_drag = state.scrollbar.clone();
            let touched = state.scrollbar_touched.clone();
            let dwell_state = state.dwell.clone();
            let dwell_task = state.dwell_task.clone();
            let dwell_cfg = self.dwell;
            let this_emulator = self.emulator.clone();
            let on_copied = self.on_selection_copied.clone();
            let file_links = self.file_links.clone();
            let link_cache = state.link_cache.clone();
            let link_hint = state.link_hint.clone();
            let link_hint_task = state.link_hint_task.clone();
            let alt_press = state.alt_press.clone();
            let autoscroll = state.autoscroll.clone();
            let autoscroll_task = state.autoscroll_task.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                // 任何一键按下都撤掉悬停提示(起选区、弹右键菜单、点链接都不该再挂着它);
                // 要再出得松手后重新停留
                link_hint_task.borrow_mut().take();
                if link_hint.borrow_mut().cancel() {
                    cx.notify(view_id);
                }
                // 上一次拖选的残局一律作废(丢了 MouseUp 的兜底):挂着的 ⌥+单击、还在跑的
                // 自动滚屏、旧拖选锚点。正常松手时这些早就收干净了,这里是空操作
                alt_press.set(None);
                drag_anchor.set(None);
                stop_autoscroll(&autoscroll, &autoscroll_task);
                let mods = modifiers_of(&event.modifiers);
                let (col, row, side) =
                    Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);

                // ── 滚动条先于一切:滑块上按下只能是拖滚动条,不能顺带起一个选区,
                //    也不能被鼠标上报吃掉(TUI 收到一个它没法解释的点击更糟)
                if let Some(layout) = bar
                    && event.button == MouseButton::Left
                {
                    match layout.hit(event.position, GRAB_SLACK) {
                        ScrollbarHit::Thumb => {
                            let grab =
                                f32::from(event.position.y) - f32::from(layout.thumb.origin.y);
                            bar_drag.set(ScrollbarDrag {
                                grab_offset: Some(grab),
                                hovered: true,
                            });
                            touched.set(Some(Instant::now()));
                            // 滑块按下只改本元素的滚动条外观
                            cx.notify(view_id);
                            return;
                        }
                        ScrollbarHit::Track => {
                            let target = layout.offset_for_track_click(event.position);
                            let delta = target as i32 - layout.display_offset as i32;
                            if delta != 0 {
                                this_emulator
                                    .with_term_mut(|t| t.scroll_display(Scroll::Delta(delta)));
                            }
                            bar_drag.set(ScrollbarDrag {
                                grab_offset: None,
                                hovered: true,
                            });
                            touched.set(Some(Instant::now()));
                            // 轨道点击 = 本元素跳一段回看位置,同上
                            cx.notify(view_id);
                            return;
                        }
                        ScrollbarHit::Miss => {}
                    }
                }

                // ── 链接:Ctrl(⌘)+左键点在链接上 = 打开。排在鼠标上报之前 ——
                //    开了上报的 TUI 里照样能点;点中了既不起选区、也不报给程序。
                //
                //    按**这一下**的位置与修饰键现识别,不读上一帧画出来的悬停态:
                //    非焦点的分屏按下 Ctrl 收不到 modifiers-changed,没动鼠标就直接点,
                //    上一帧根本没画过下划线。
                if event.button == MouseButton::Left
                    && event.modifiers.secondary(cx)
                    && let Some(links) = file_links.as_ref()
                {
                    let display_offset = emulator.with_term(|t| t.grid().display_offset());
                    let line = row as i32 - display_offset as i32;
                    let hit = links::probe(
                        &emulator,
                        links,
                        line,
                        col,
                        &mut link_cache.borrow_mut(),
                        cx,
                    );
                    if let Some(hit) = hit {
                        match &hit.target {
                            LinkTarget::File(link) => (links.open)(link, window, cx),
                            LinkTarget::Url { url, .. } => (links.open_url)(url, cx),
                        }
                        // 视图那层的「左键聚焦终端」不必再跑:焦点马上要交给文件页 / 浏览器
                        cx.stop_propagation();
                        return;
                    }
                }

                if !prefers_local_handling(mode, mods) {
                    let Some(btn) = map_button(event.button) else {
                        return;
                    };
                    if let Some(on_input) = on_input.as_ref()
                        && let Some(bytes) = mouse_report_bytes(
                            mode,
                            MouseAction::Press(btn),
                            mods,
                            GridPos::new(col, row),
                        )
                    {
                        on_input(&bytes, window, cx);
                    }
                    reported.set(Some(btn));
                    last_cell.set(Some((col, row)));
                    // 程序接管鼠标了,残留的本地高亮只会让人误以为还能复制
                    emulator.with_term_mut(|term| term.selection = None);
                    selecting.set(false);
                    drag_anchor.set(None);
                    // 抹掉的是本元素的高亮,别的面板没读过选区
                    cx.notify(view_id);
                    return;
                }

                // 本地:左键。这一下是扩选、⌥+单击还是新起选区(单击普通 / 双击选词 /
                // 三击选行),优先级见 `drag_select` 模块注释
                if event.button != MouseButton::Left {
                    return;
                }
                let (display_offset, history, alt_screen, has_selection) =
                    emulator.with_term(|t| {
                        // 「手上有没有看得见的选区」只有按着 Shift 时用得上
                        let has_selection = event.modifiers.shift
                            && t.selection
                                .as_ref()
                                .is_some_and(|sel| sel.to_range(t).is_some());
                        (
                            t.grid().display_offset(),
                            t.grid().history_size(),
                            t.mode().contains(TermMode::ALT_SCREEN),
                            has_selection,
                        )
                    });
                // ⌥+单击定位光标只在**没滚动回看**时生效:滚上去之后视口行号与光标所在的
                // grid 行不是一回事,差值算出来会把光标带到莫名其妙的地方
                let cursor_move = display_offset == 0 && on_input.is_some();
                let point = Self::grid_point(col, row, display_offset);
                match classify_left_press(
                    event.click_count,
                    event.modifiers.shift,
                    event.modifiers.alt,
                    has_selection,
                    cursor_move,
                ) {
                    // ⌥+单击:把光标挪到点中的格子(判定与合成见 [`cursor_move_bytes`])。
                    // **修饰键不能省**:裸左键是拖选区的起手式,两者抢同一个手势;
                    // Terminal.app 同样挂在 ⌥+click 上,这里照它。
                    //
                    // 按下时先挂起:⌥+拖动是块选,得等松手才知道这一下是不是单击
                    // (拖过一格就在 mouse-move 那边转块选)。选区与停留复制这一下都不碰
                    // —— 与从前的 ⌥+单击一样,已有的选区不被清掉
                    LeftPress::AltClick => {
                        alt_press.set(Some(AltPress {
                            position: event.position,
                            col,
                            row,
                            anchor: point,
                            side,
                            history,
                            alt_screen,
                        }));
                        return;
                    }
                    // Shift+单击扩选:锚点与选区类型不动,只把活动端挪过来;接着拖照样延伸
                    LeftPress::Extend => {
                        emulator.with_term_mut(|term| {
                            if let Some(sel) = term.selection.as_mut() {
                                sel.update(point, side);
                            }
                        });
                        // 扩选的真实锚点在 Selection 私有字段里,不另存副本避免角点反转
                        drag_anchor.set(None);
                        selecting.set(true);
                    }
                    LeftPress::Start(ty) => {
                        let anchor = emulator.with_term_mut(|term| {
                            term.selection = Some(Selection::new(ty, point, side));
                            DragAnchor {
                                ty,
                                point,
                                side,
                                history: term.grid().history_size(),
                                alt_screen: term.mode().contains(TermMode::ALT_SCREEN),
                            }
                        });
                        // 双击 / 三击选出的词与行不跟着拖(与从前一致)
                        let drag = event.click_count == 1;
                        selecting.set(drag);
                        drag_anchor.set(drag.then_some(anchor));
                    }
                }

                // ── 拖选停留复制:按下就起表(停留语义关掉时 on_press 返回 None)
                // 存**元素相对**坐标:气泡落点要按元素宽度贴边收拢
                //(原版是 `lastX - rect.left`),存绝对坐标会让分屏右侧的终端算歪
                let generation = dwell_state.borrow_mut().on_press(
                    &dwell_cfg,
                    (
                        f32::from(event.position.x - origin.x),
                        f32::from(event.position.y - origin.y),
                    ),
                );
                if let Some(generation) = generation {
                    *dwell_task.borrow_mut() = Some(arm_dwell(
                        generation,
                        dwell_cfg,
                        dwell_state.clone(),
                        this_emulator.clone(),
                        on_copied.clone(),
                        element_size,
                        view_id,
                        window,
                        cx,
                    ));
                }
                // 起了个新选区,重画的只有本元素
                cx.notify(view_id);
            });
        }
    }

    fn bind_mouse_move_listener(
        &self,
        bind: &MouseBindCtx,
        scroll_ctx: AutoScrollCtx,
        window: &mut Window,
    ) {
        let MouseBindCtx {
            view_id,
            origin,
            cell_size,
            columns,
            screen_lines,
            mode,
            alt_screen,
            bar,
            element_size,
            ..
        } = *bind;
        let hitbox = &bind.hitbox;
        let state = &bind.state;
        // ── 移动:上报拖动 / 上报纯移动 / 延伸本地选择
        {
            let emulator = self.emulator.clone();
            let hitbox = hitbox.clone();
            let selecting = state.selecting.clone();
            let drag_anchor = state.drag_anchor.clone();
            let reported = state.reported_button.clone();
            let last_cell = state.last_reported_cell.clone();
            let on_input = self.on_input.clone();
            let bar_drag = state.scrollbar.clone();
            let touched = state.scrollbar_touched.clone();
            let dwell_state = state.dwell.clone();
            let dwell_task = state.dwell_task.clone();
            let dwell_cfg = self.dwell;
            let on_copied = self.on_selection_copied.clone();
            let this_emulator = self.emulator.clone();
            let has_links = self.file_links.is_some();
            let link_pointer = state.link_pointer.clone();
            // 悬停提示:接了文案和链接能力才做
            let hint_links = self.link_hint.as_ref().and(self.file_links.clone());
            let link_hint = state.link_hint.clone();
            let link_hint_task = state.link_hint_task.clone();
            let link_hint_cache = state.link_hint_cache.clone();
            let alt_press = state.alt_press.clone();
            let autoscroll = state.autoscroll.clone();
            let autoscroll_task = state.autoscroll_task.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }

                // ── 链接:按住 Ctrl 跨格、松开、移出元素时要一帧(悬停态在 paint 里
                //    现算)。不按 Ctrl 的移动这里只比一次 `None == None`,不要帧。
                if has_links {
                    let cell = (event.modifiers.secondary(cx)
                        && event.pressed_button.is_none()
                        && hitbox.is_hovered(window))
                    .then(|| {
                        let (col, row, _) = Self::hit_cell(
                            event.position,
                            origin,
                            cell_size,
                            columns,
                            screen_lines,
                        );
                        (col, row)
                    });
                    if link_pointer.replace(cell) != cell {
                        cx.notify(view_id);
                    }
                }

                // ── 不按修饰键的悬停提示:跨格只换停留计时器,不探、不要帧;挂着的
                //    提示被移开时才要一帧擦掉(口径见 `super::links` 模块注释)。
                //    TUI 抓着鼠标、正在拖选 / 拖滚动条、按着 Ctrl、悬在滚动条上都不提示
                if let Some(links) = hint_links.as_ref() {
                    let on_bar = bar.is_some_and(|layout| {
                        layout.hit(event.position, GRAB_SLACK) != ScrollbarHit::Miss
                    });
                    let cell = (!event.modifiers.secondary(cx)
                        && event.pressed_button.is_none()
                        && !selecting.get()
                        && reported.get().is_none()
                        && !mouse_reporting_active(mode)
                        && !on_bar
                        && hitbox.is_hovered(window))
                    .then(|| {
                        let (col, row, _) = Self::hit_cell(
                            event.position,
                            origin,
                            cell_size,
                            columns,
                            screen_lines,
                        );
                        (col, row)
                    });
                    let step = link_hint.borrow_mut().on_move(cell);
                    if let Some(generation) = step.rearm {
                        *link_hint_task.borrow_mut() = Some(arm_link_hint(
                            generation,
                            link_hint.clone(),
                            link_hint_cache.clone(),
                            this_emulator.clone(),
                            links.clone(),
                            view_id,
                            window,
                            cx,
                        ));
                    } else if step.disarm {
                        link_hint_task.borrow_mut().take();
                    }
                    if step.repaint {
                        cx.notify(view_id);
                    }
                }

                // ── 拖着滑块:全局跟手,拖出元素也继续(和所有滚动条一样)
                if let Some(layout) = bar {
                    let drag = bar_drag.get();
                    if let Some(grab) = drag.grab_offset {
                        let target =
                            layout.offset_for_thumb_top(px(f32::from(event.position.y) - grab));
                        let delta = target as i32 - layout.display_offset as i32;
                        if delta != 0 {
                            this_emulator.with_term_mut(|t| t.scroll_display(Scroll::Delta(delta)));
                        }
                        touched.set(Some(Instant::now()));
                        // 拖滑块是最热的一条路(每个 mouse-move 一发),
                        // 更不能让三块 cached 面板跟着重排
                        cx.notify(view_id);
                        return;
                    }
                    let hovered = hitbox.is_hovered(window)
                        && layout.hit(event.position, GRAB_SLACK) != ScrollbarHit::Miss;
                    if hovered != drag.hovered {
                        bar_drag.set(ScrollbarDrag { hovered, ..drag });
                        touched.set(Some(Instant::now()));
                        // 滑块 hover 态是本元素自己的,不是全局 hover
                        cx.notify(view_id);
                    }
                    // 悬在条上时不要顺手延伸选区;已经在拖选的除外(选区拖过条子底下)
                    if hovered && !selecting.get() {
                        return;
                    }
                } else if bar_drag.get() != ScrollbarDrag::default() {
                    bar_drag.set(ScrollbarDrag::default());
                }

                // ── 停留复制:越过 4px 阈值才重新计时
                let rearm = dwell_state.borrow_mut().on_move(
                    &dwell_cfg,
                    (
                        f32::from(event.position.x - origin.x),
                        f32::from(event.position.y - origin.y),
                    ),
                );
                if let Some(generation) = rearm {
                    *dwell_task.borrow_mut() = Some(arm_dwell(
                        generation,
                        dwell_cfg,
                        dwell_state.clone(),
                        this_emulator.clone(),
                        on_copied.clone(),
                        element_size,
                        view_id,
                        window,
                        cx,
                    ));
                }

                // ── ⌥+按下还没转块选:拖满一格才转;没满什么都不做(松手按单击定位光标)
                if let Some(press) = alt_press.get() {
                    if event.pressed_button != Some(MouseButton::Left) {
                        // 丢了 MouseUp:这一下作废,既不定位光标也不开选区
                        alt_press.set(None);
                        return;
                    }
                    let dx = f(event.position.x - press.position.x);
                    let dy = f(event.position.y - press.position.y);
                    if !drag_exceeds_cell(dx, dy, f(cell_size.width), f(cell_size.height)) {
                        return;
                    }
                    alt_press.set(None);
                    let anchor = DragAnchor {
                        ty: SelectionType::Block,
                        point: press.anchor,
                        side: press.side,
                        history: press.history,
                        alt_screen: press.alt_screen,
                    };
                    emulator.with_term_mut(|term| {
                        term.selection = None;
                        update_or_restore_selection(term, Some(anchor), press.anchor, press.side);
                    });
                    selecting.set(true);
                    drag_anchor.set(Some(anchor));
                    // 停留复制从转块选这一刻起算:按下那会儿还不知道这一下是不是在选
                    let generation = dwell_state.borrow_mut().on_press(
                        &dwell_cfg,
                        (
                            f32::from(event.position.x - origin.x),
                            f32::from(event.position.y - origin.y),
                        ),
                    );
                    if let Some(generation) = generation {
                        *dwell_task.borrow_mut() = Some(arm_dwell(
                            generation,
                            dwell_cfg,
                            dwell_state.clone(),
                            this_emulator.clone(),
                            on_copied.clone(),
                            element_size,
                            view_id,
                            window,
                            cx,
                        ));
                    }
                    // 接着往下走:把活动端延伸到当前指针
                }

                let mods = modifiers_of(&event.modifiers);
                let held = reported.get();

                if held.is_some() || (mouse_reporting_active(mode) && !selecting.get()) {
                    // 纯移动上报(1003)要求指针真的在元素上;拖动则允许拖出去
                    if held.is_none() && !hitbox.is_hovered(window) {
                        return;
                    }
                    let (col, row, _) =
                        Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);
                    // 跨格才发。不然一个像素一条消息,TUI 那头光解析就跑满一个核。
                    if last_cell.get() == Some((col, row)) {
                        return;
                    }
                    if let Some(on_input) = on_input.as_ref()
                        && let Some(bytes) = mouse_report_bytes(
                            mode,
                            MouseAction::Motion(held),
                            mods,
                            GridPos::new(col, row),
                        )
                    {
                        last_cell.set(Some((col, row)));
                        on_input(&bytes, window, cx);
                        return;
                    }
                    if held.is_some() {
                        // 拖动中但这次不该报(模式只有 1000):记住格子,别漏掉后面的松开配对
                        last_cell.set(Some((col, row)));
                        return;
                    }
                }

                if !selecting.get() || event.pressed_button != Some(MouseButton::Left) {
                    // 没在拖选(或按着的已经不是左键 —— 丢了 MouseUp):自动滚屏与拖选锚点一并收干净。
                    // 没在跑时只是一次 Cell 读
                    selecting.set(false);
                    drag_anchor.set(None);
                    stop_autoscroll(&autoscroll, &autoscroll_task);
                    return;
                }
                // 拖出元素外也要继续选,所以这里不判 hover
                let (col, row, side) =
                    Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);
                let anchor = drag_anchor.get();
                let (display_offset, history) = emulator.with_term_mut(|term| {
                    let display_offset = term.grid().display_offset();
                    let history = term.grid().history_size();
                    update_or_restore_selection(
                        term,
                        anchor,
                        Self::grid_point(col, row, display_offset),
                        side,
                    );
                    (display_offset, history)
                });
                // ── 拖到可视区上 / 下边外:起自动滚屏;回到区内、滚到头:停。指针停在区外
                //    不动时没有 mouse-move 可等,节拍任务自己读这里记下的指针
                let mut shared = autoscroll.get();
                shared.pointer = event.position;
                autoscroll.set(shared);
                // alt screen 没有回看(`history` 恒 0 已经挡住,这里再按模式明说一次)
                let wants = !alt_screen
                    && autoscroll_lines(f(event.position.y), shared.zone, display_offset, history)
                        != 0;
                if wants {
                    start_autoscroll(&scroll_ctx, &autoscroll_task, window, cx);
                } else {
                    stop_autoscroll(&autoscroll, &autoscroll_task);
                }
                // ⚠️ 这是最热的一处:拖选一次 = 每个 mouse-move 一发。
                // 原来的 refresh 会让三块 cached 面板逐帧重排,改用 notify 之后
                // 只有这个终端视图重画
                cx.notify(view_id);
            });
        }
    }

    fn bind_mouse_up_listener(&self, bind: &MouseBindCtx, window: &mut Window) {
        let MouseBindCtx {
            view_id,
            origin,
            cell_size,
            columns,
            screen_lines,
            mode,
            ..
        } = *bind;
        let state = &bind.state;
        // ── 松开:配对上报,或结束拖选并把选中文本送进剪贴板
        //    (X11 primary selection 的习惯;Ctrl+Shift+C 由宿主再走一遍也无妨)
        {
            let emulator = self.emulator.clone();
            let selecting = state.selecting.clone();
            let drag_anchor = state.drag_anchor.clone();
            let reported = state.reported_button.clone();
            let last_cell = state.last_reported_cell.clone();
            let on_input = self.on_input.clone();
            let bar_drag = state.scrollbar.clone();
            let touched = state.scrollbar_touched.clone();
            let dwell_state = state.dwell.clone();
            let dwell_task = state.dwell_task.clone();
            let dwell_cfg = self.dwell;
            let alt_press = state.alt_press.clone();
            let autoscroll = state.autoscroll.clone();
            let autoscroll_task = state.autoscroll_task.clone();
            window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }

                // ── 松开滑块。**不判 hover**:拖到窗口外松手也得收尾,
                //    否则滑块会一直粘在鼠标上
                let drag = bar_drag.get();
                if drag.grab_offset.is_some() && event.button == MouseButton::Left {
                    bar_drag.set(ScrollbarDrag {
                        grab_offset: None,
                        ..drag
                    });
                    touched.set(Some(Instant::now()));
                    // 松开滑块只改本元素的滑块态与淡出计时
                    cx.notify(view_id);
                    return;
                }
                // 按下时上报过的键,松开必须配对上报 —— 否则 TUI 会一直以为鼠标还按着。
                // **不看当前 mode**:程序可能在按住期间关掉了上报模式,那也得把这一次收尾。
                if let Some(btn) = reported.get()
                    && map_button(event.button) == Some(btn)
                {
                    reported.set(None);
                    last_cell.set(None);
                    let (col, row, _) =
                        Self::hit_cell(event.position, origin, cell_size, columns, screen_lines);
                    // shift 在这里必须抹掉:按住期间**中途按下 Shift** 会让
                    // `prefers_local_handling` 把松开事件吞掉,TUI 从此认为
                    // 鼠标一直按着(拖动框永远不结束)
                    let release_mods = MouseMods {
                        shift: false,
                        ..modifiers_of(&event.modifiers)
                    };
                    if let Some(on_input) = on_input.as_ref()
                        && let Some(bytes) = mouse_report_bytes(
                            mode,
                            MouseAction::Release(btn),
                            release_mods,
                            GridPos::new(col, row),
                        )
                    {
                        on_input(&bytes, window, cx);
                    }
                    return;
                }

                if event.button != MouseButton::Left {
                    return;
                }
                // 松手 = 拖选结束,自动滚屏当场停(drop 句柄即取消节拍任务)
                stop_autoscroll(&autoscroll, &autoscroll_task);
                // ⌥+单击(按下后没拖满一格就松手):把光标挪到按下的那一格。判定在按下时已经
                // 做完,这里只是把合成推迟到「确定不是拖」之后;按住期间滚过回看的话行号已经
                // 对不上光标所在行,作罢
                if let Some(press) = alt_press.take() {
                    let (display_offset, (cur_line, cur_col)) = emulator.with_term(|t| {
                        let p = t.grid().cursor.point;
                        (t.grid().display_offset(), (p.line.0, p.column.0 as i32))
                    });
                    if display_offset == 0
                        && let Some(on_input) = on_input.as_ref()
                    {
                        let payload = cursor_move_bytes(
                            cur_line,
                            cur_col,
                            press.row as i32,
                            press.col as i32,
                            mode,
                        );
                        if !payload.is_empty() {
                            on_input(&payload, window, cx);
                        }
                    }
                    return;
                }
                let was_selecting = selecting.replace(false);
                drag_anchor.set(None);
                // 定时器一律作废:松手之后不该再弹「已复制」
                dwell_task.borrow_mut().take();
                let action = dwell_state.borrow_mut().on_release(&dwell_cfg);
                match action {
                    // 停留语义关掉 = 旧行为,松手即复制(X11 primary selection 的习惯)
                    ReleaseAction::CopyNow if was_selecting => {
                        if let Some(text) = emulator.with_term(|t| t.selection_to_string())
                            && !text.is_empty()
                        {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                        }
                    }
                    // 停留期间已经复制过一次:拖到边缘触发自动滚屏时选区可能还在长,
                    // 松手时若已变化就补一刀,让剪贴板与用户最终看到的选区一致。
                    // **不重弹气泡** —— 「已复制」对最终内容依然成立。
                    ReleaseAction::ReconcileIfChanged => {
                        let text = emulator
                            .with_term(|t| t.selection_to_string())
                            .unwrap_or_default();
                        if !text.is_empty() && dwell_state.borrow().needs_reconcile(&text) {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                            dwell_state.borrow_mut().note_copied(text);
                        }
                    }
                    _ => {}
                }
            });
        }
    }

    fn bind_mouse_exit_listener(&self, bind: &MouseBindCtx, window: &mut Window) {
        let view_id = bind.view_id;
        let state = &bind.state;
        // ── 鼠标离开窗口:mouse-move 不会再来,挂着的悬停提示在这里撤
        if self.link_hint.is_some() {
            let link_hint = state.link_hint.clone();
            let link_hint_task = state.link_hint_task.clone();
            window.on_mouse_event(move |_: &MouseExitEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                link_hint_task.borrow_mut().take();
                if link_hint.borrow_mut().cancel() {
                    cx.notify(view_id);
                }
            });
        }
    }
}

/// 起一个悬停提示的停留计时器(口径见 [`super::links`] 模块注释「不按修饰键的悬停提示」)。
/// 返回的 `Task` 必须由元素状态持有 —— 换格时覆盖掉旧句柄就是取消。
///
/// 到点才探:代号对不上(期间鼠标动过)、TUI 开了鼠标上报、按下了 Ctrl 都作罢;探不到
/// 链接也作罢 —— **这几条路都不要帧**,只有真挂上提示才 notify 一次。每次到点都先清掉
/// 裁决缓存再验盘:刚生成的文件这一次就认得出。
#[allow(clippy::too_many_arguments)]
pub(super) fn arm_link_hint(
    generation: u64,
    hint: Rc<RefCell<LinkHint>>,
    cache: Rc<RefCell<LinkCache>>,
    emulator: Arc<TerminalEmulator>,
    file_links: FileLinks,
    view_id: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Task<()> {
    window.spawn(cx, async move |cx| {
        cx.background_executor().timer(LINK_HINT_DELAY).await;
        let _ = cx.update(|window, cx| {
            let Some((col, row)) = hint.borrow().elapsed(generation) else {
                return;
            };
            if window.modifiers().secondary(cx) || mouse_reporting_active(emulator.mode()) {
                return;
            }
            let (display_offset, content_generation) =
                emulator.with_term(|term| (term.grid().display_offset(), emulator.generation()));
            let line = row as i32 - display_offset as i32;
            let link = {
                let mut cache = cache.borrow_mut();
                cache.clear();
                links::probe(&emulator, &file_links, line, col, &mut cache, cx)
            };
            let Some(link) = link else {
                return;
            };
            if hint
                .borrow_mut()
                .show(generation, link, display_offset, content_generation)
            {
                cx.notify(view_id);
            }
        });
    })
}

/// 起一个 dwell 定时器。返回的 `Task` **必须被宿主持有** —— 一 drop 就取消,
/// 这正好是「鼠标动了要重新计时」的实现方式(直接覆盖掉旧句柄)。
///
/// 代号(`generation`)是第二道闸:定时器已经飞出去、句柄又被覆盖不掉的竞态下,
/// 回来的那一发靠代号自己认赔。
///
/// `view_id` 由调用方在 paint 期取好传进来:定时器回来时已经不在绘制期,
/// `window.current_view()` 那条断言过不去。
#[allow(clippy::too_many_arguments)]
pub(super) fn arm_dwell(
    generation: u64,
    cfg: DwellConfig,
    tracker: Rc<RefCell<DwellTracker>>,
    emulator: Arc<TerminalEmulator>,
    on_copied: Option<OnSelectionCopied>,
    element_size: (f32, f32),
    view_id: EntityId,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Task<()> {
    let delay = cfg.dwell;
    window.spawn(cx, async move |cx| {
        cx.background_executor().timer(delay).await;
        let _ = cx.update(|window, cx| {
            let text = emulator
                .with_term(|t| t.selection_to_string())
                .unwrap_or_default();
            if !tracker
                .borrow()
                .on_dwell_elapsed(generation, !text.is_empty())
            {
                return;
            }
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            let origin = tracker.borrow().tip_origin(element_size);
            tracker.borrow_mut().note_copied(text.clone());
            if let Some(cb) = on_copied.as_ref() {
                // 落点是**元素相对**坐标(tip_origin 已按元素尺寸贴边收拢):
                // 宿主把气泡 absolute 放在终端容器里就地对得上
                cb(&text, origin, window, cx);
            }
            // 宿主那条回调自己会 notify 它的视图(「已复制」气泡挂在 pane 上),
            // 这里只管本元素:选区高亮与停留态变了
            cx.notify(view_id);
        });
    })
}

/// 自动滚屏节拍任务要带进去的东西:全是跨帧存活的共享格子,**不含任务句柄本身**
/// (见 [`TerminalElementState::autoscroll_task`] 那条循环引用的提醒)。
#[derive(Clone)]
pub(super) struct AutoScrollCtx {
    pub(super) emulator: Arc<TerminalEmulator>,
    pub(super) selecting: Rc<StdCell<bool>>,
    pub(super) drag_anchor: Rc<StdCell<Option<DragAnchor>>>,
    pub(super) shared: Rc<StdCell<AutoScroll>>,
    /// 滚动条的「最近碰过」时刻:滚了就让它亮起来。
    pub(super) touched: Rc<StdCell<Option<Instant>>>,
    pub(super) dwell: Rc<RefCell<DwellTracker>>,
    pub(super) dwell_task: Rc<RefCell<Option<gpui::Task<()>>>>,
    pub(super) dwell_cfg: DwellConfig,
    pub(super) on_copied: Option<OnSelectionCopied>,
    pub(super) element_size: (f32, f32),
    pub(super) view_id: EntityId,
}

/// 起自动滚屏节拍。已经在跑就什么都不做(mouse-move 一个像素一发,不能每发都重起一个)。
pub(super) fn start_autoscroll(
    ctx: &AutoScrollCtx,
    task: &RefCell<Option<gpui::Task<()>>>,
    window: &mut Window,
    cx: &mut App,
) {
    let mut shared = ctx.shared.get();
    if shared.running {
        return;
    }
    shared.running = true;
    ctx.shared.set(shared);
    *task.borrow_mut() = Some(arm_autoscroll(ctx.clone(), window, cx));
}

/// 停自动滚屏:放下标记、drop 句柄(= 取消节拍任务)。没在跑时只是一次 Cell 读。
pub(super) fn stop_autoscroll(
    shared: &StdCell<AutoScroll>,
    task: &RefCell<Option<gpui::Task<()>>>,
) {
    let mut state = shared.get();
    if !state.running {
        return;
    }
    state.running = false;
    shared.set(state);
    task.borrow_mut().take();
}

/// 自动滚屏的节拍任务:每 [`AUTOSCROLL_TICK`] 滚一拍,该停时自己退出并放下标记 ——
/// 退出之后不留任何定时器。返回的 `Task` 必须由元素状态持有(drop = 取消)。
pub(super) fn arm_autoscroll(
    ctx: AutoScrollCtx,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Task<()> {
    window.spawn(cx, async move |cx| {
        loop {
            cx.background_executor().timer(AUTOSCROLL_TICK).await;
            let keep = cx
                .update(|window, cx| autoscroll_tick(&ctx, window, cx))
                .unwrap_or(false);
            if !keep {
                let mut shared = ctx.shared.get();
                shared.running = false;
                ctx.shared.set(shared);
                return;
            }
        }
    })
}

/// 自动滚屏的一拍:朝指针那边滚 [`autoscroll_lines`] 行,把选区活动端落到新露出的边行上,
/// 停留复制重新计时。返回 `false` = 该停了:松了手 / 选区没了 / alt screen / 窗口不在前台
/// (焦点被别的窗口抢走时 MouseUp 可能永远不来,不停就一路滚到头)/ 指针回到区内 / 滚到头。
pub(super) fn autoscroll_tick(ctx: &AutoScrollCtx, window: &mut Window, cx: &mut App) -> bool {
    let shared = ctx.shared.get();
    if !shared.running || !ctx.selecting.get() || !window.is_window_active() {
        return false;
    }
    let anchor = ctx.drag_anchor.get();
    let can_restore = anchor.is_some_and(|a| !a.alt_screen);
    let moved = ctx.emulator.with_term_mut(|term| {
        if term.mode().contains(TermMode::ALT_SCREEN) || (term.selection.is_none() && !can_restore)
        {
            return false;
        }
        let lines = {
            let grid = term.grid();
            autoscroll_lines(
                f(shared.pointer.y),
                shared.zone,
                grid.display_offset(),
                grid.history_size(),
            )
        };
        if lines == 0 {
            return false;
        }
        term.scroll_display(Scroll::Delta(lines));
        let display_offset = term.grid().display_offset();
        // 指针在区外:行号钳到边行,列照指针横坐标走(块选的右边沿跟着手)
        let (col, row, side) = TerminalElement::hit_cell(
            shared.pointer,
            shared.origin,
            shared.cell_size,
            shared.columns,
            shared.screen_lines,
        );
        update_or_restore_selection(
            term,
            anchor,
            TerminalElement::grid_point(col, row, display_offset),
            side,
        );
        true
    });
    if !moved {
        return false;
    }
    // 滚了就把滚动条亮起来(与滚轮同一口径)
    ctx.touched.set(Some(Instant::now()));
    // 鼠标没动、选区却在长:停留复制重新计时,别在半截时复制
    let generation = ctx.dwell.borrow_mut().restart(&ctx.dwell_cfg);
    if let Some(generation) = generation {
        *ctx.dwell_task.borrow_mut() = Some(arm_dwell(
            generation,
            ctx.dwell_cfg,
            ctx.dwell.clone(),
            ctx.emulator.clone(),
            ctx.on_copied.clone(),
            ctx.element_size,
            ctx.view_id,
            window,
            cx,
        ));
    }
    cx.notify(ctx.view_id);
    true
}

/// 查找的兜底重绘:`delay` 之后 notify 一次本视图,让下一帧的 prepaint 再 sync
/// 一次(见 [`TerminalSearch::take_trailing_rescan`])。
///
/// 任务直接 `detach`:防重靠引擎里的「已排」标记,不靠持有句柄;pane 先关掉了,
/// 到点时 notify 一个已经不在的视图是空操作。
pub(super) fn schedule_search_rescan(
    search: Rc<RefCell<TerminalSearch>>,
    delay: Duration,
    view_id: EntityId,
    window: &mut Window,
    cx: &mut App,
) {
    window
        .spawn(cx, async move |cx| {
            cx.background_executor().timer(delay).await;
            search.borrow_mut().trailing_rescan_fired();
            let _ = cx.update(|_, cx| cx.notify(view_id));
        })
        .detach();
}

/// gpui 的按键 → 协议按键。没有对应编码的一律丢弃。
pub(super) fn map_button(button: MouseButton) -> Option<MouseBtn> {
    match button {
        MouseButton::Left => Some(MouseBtn::Left),
        MouseButton::Middle => Some(MouseBtn::Middle),
        MouseButton::Right => Some(MouseBtn::Right),
        // 侧键:gpui 给的是 0/1(后退/前进),协议里是 8/9
        MouseButton::Navigate(gpui::NavigationDirection::Back) => Some(MouseBtn::Other(8)),
        MouseButton::Navigate(gpui::NavigationDirection::Forward) => Some(MouseBtn::Other(9)),
    }
}

pub(super) fn modifiers_of(m: &gpui::Modifiers) -> MouseMods {
    MouseMods::new(m.shift, m.alt, m.control)
}
