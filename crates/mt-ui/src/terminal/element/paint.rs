use super::*;

pub(super) fn paint_hollow_rect(window: &mut Window, bounds: Bounds<Pixels>, color: Hsla) {
    let t = px(1.0);
    let Bounds { origin, size: s } = bounds;
    window.paint_quad(fill(Bounds::new(origin, size(s.width, t)), color));
    window.paint_quad(fill(
        Bounds::new(point(origin.x, origin.y + s.height - t), size(s.width, t)),
        color,
    ));
    window.paint_quad(fill(Bounds::new(origin, size(t, s.height)), color));
    window.paint_quad(fill(
        Bounds::new(point(origin.x + s.width - t, origin.y), size(t, s.height)),
        color,
    ));
}

impl TerminalElement {
    /// 像素坐标 → 可视区行列。行号是**屏幕行**(0 = 最上面那行),与 display_offset 无关。
    pub(super) fn hit_cell(
        pos: Point<Pixels>,
        origin: Point<Pixels>,
        cell_size: Size<Pixels>,
        columns: usize,
        screen_lines: usize,
    ) -> (usize, usize, Side) {
        let rel_x = f(pos.x - origin.x).max(0.0);
        let rel_y = f(pos.y - origin.y).max(0.0);
        let col_f = rel_x / f(cell_size.width).max(1.0);
        let row_f = rel_y / f(cell_size.height).max(1.0);
        let col = (col_f.floor() as usize).min(columns.saturating_sub(1));
        let row = (row_f.floor() as usize).min(screen_lines.saturating_sub(1));
        let side = if col_f - col_f.floor() > 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        (col, row, side)
    }

    /// 屏幕行列 → alacritty 的 grid 坐标(选择区要用)。
    pub(super) fn grid_point(col: usize, row: usize, display_offset: usize) -> AlacPoint {
        AlacPoint::new(Line(row as i32 - display_offset as i32), Column(col))
    }

    /// 这一帧鼠标底下的链接(文件或网址)。**只能在 paint 里调**:`hitbox.is_hovered` 认的是
    /// 本帧的命中测试,prepaint 时它还是上一帧的。
    ///
    /// 不在链接态(没按 Ctrl / 鼠标不在终端上 / 正在拖选或拖滚动条 / 按键正被上报给
    /// TUI)时顺手清掉裁决缓存,下次按下 Ctrl 时刚生成的文件也认得出来。
    pub(super) fn hovered_link(
        &self,
        prepared: &PreparedFrame,
        window: &Window,
        cx: &App,
    ) -> Option<HoveredLink> {
        let links = self.file_links.as_ref()?;
        let state = &prepared.state;
        let position = window.mouse_position();
        let on_bar = prepared
            .scrollbar
            .is_some_and(|bar| bar.hit(position, GRAB_SLACK) != ScrollbarHit::Miss);
        let active = window.modifiers().secondary(cx)
            && prepared.hitbox.is_hovered(window)
            && !on_bar
            && !state.selecting.get()
            && state.reported_button.get().is_none()
            && state.scrollbar.get().grab_offset.is_none();
        if !active {
            state.link_cache.borrow_mut().clear();
            return None;
        }
        let (col, row, _) = Self::hit_cell(
            position,
            prepared.origin,
            prepared.cell_size,
            prepared.columns,
            prepared.screen_lines,
        );
        let line = row as i32 - prepared.display_offset as i32;
        links::probe(
            &self.emulator,
            links,
            line,
            col,
            &mut state.link_cache.borrow_mut(),
            cx,
        )
    }

    /// 这一帧挂着的悬停提示(不按修饰键、停留够了的那条)。**只能在 paint 里调**,
    /// 理由同 [`Self::hovered_link`]。
    ///
    /// 判据与 mouse-move 那边起停留计时的条件同一套。不满足时**不撤状态**:下一次
    /// 鼠标移动会撤,这里撤了反而要补一帧。内容变了才重探(见 [`LinkHint::current`])。
    pub(super) fn hinted_link(
        &self,
        prepared: &PreparedFrame,
        window: &Window,
        cx: &App,
    ) -> Option<HoveredLink> {
        self.link_hint.as_ref()?;
        let links = self.file_links.as_ref()?;
        let state = &prepared.state;
        let position = window.mouse_position();
        let on_bar = prepared
            .scrollbar
            .is_some_and(|bar| bar.hit(position, GRAB_SLACK) != ScrollbarHit::Miss);
        let eligible = !window.modifiers().secondary(cx)
            && prepared.hitbox.is_hovered(window)
            && !on_bar
            && !state.selecting.get()
            && state.reported_button.get().is_none()
            && state.scrollbar.get().grab_offset.is_none()
            && !mouse_reporting_active(prepared.mode);
        if !eligible {
            return None;
        }
        let display_offset = prepared.display_offset;
        let content_generation = self.emulator.generation();
        let mut cache = state.link_hint_cache.borrow_mut();
        state
            .link_hint
            .borrow_mut()
            .current(display_offset, content_generation, |(col, row)| {
                let line = row as i32 - display_offset as i32;
                links::probe(&self.emulator, links, line, col, &mut cache, cx)
            })
    }

    /// 链接下划线:每段一条 1px 的线,贴着格子底边往上 2px(与 IME 预编辑串的
    /// 下划线同一粗细)。颜色由调用方给,取的都是前景色 —— 链接文字本身可能是任何颜色,
    /// 前景色在默认背景上的对比度由主题保证;悬停提示那条再压低透明度。
    pub(super) fn paint_link_underline(
        &self,
        prepared: &PreparedFrame,
        link: &HoveredLink,
        color: Hsla,
        window: &mut Window,
    ) {
        let cell = prepared.cell_size;
        for span in &link.spans {
            let row = span.line + prepared.display_offset as i32;
            if row < 0 || row as usize >= prepared.screen_lines {
                continue;
            }
            let end = span.end.min(prepared.columns);
            if end <= span.start {
                continue;
            }
            window.paint_quad(fill(
                Bounds::new(
                    point(
                        prepared.origin.x + cell.width * span.start as f32,
                        prepared.origin.y + cell.height * (row + 1) as f32 - px(2.0),
                    ),
                    size(cell.width * (end - span.start) as f32, px(1.0)),
                ),
                color,
            ));
        }
    }

    /// 链接旁边的气泡:OSC 8 的真实目标(`target`)和 / 或悬停提示那行字(`footer`)。
    ///
    /// 目标:显示的文字可以写成任何样子,悬停时必须看得到实际要打开的是什么(见
    /// [`super::links`] 模块注释),按列折行、留头留尾。提示:不折行,放最后一行;
    /// 与目标同框时压暗一档,两样东西挤在一个气泡里也分得出主次 —— 不叠两个气泡。
    /// 样式照「已复制」气泡;字体用终端字体,折行才能按列算。两样都没有就不画。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn paint_link_bubble(
        &self,
        prepared: &PreparedFrame,
        link: &HoveredLink,
        target: Option<&str>,
        footer: Option<&str>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        const PAD_X: Pixels = px(8.0);
        const PAD_Y: Pixels = px(4.0);
        const MAX_ROWS: usize = 6;
        /// 与目标同框时提示行的透明度。
        const FOOTER_ALPHA: f32 = 0.7;

        if target.is_none() && footer.is_none() {
            return;
        }

        let cell = prepared.cell_size;
        let screen_row = |line: i32| {
            let row = line + prepared.display_offset as i32;
            (row >= 0 && (row as usize) < prepared.screen_lines).then_some(row as f32)
        };
        let Some((first, first_row)) = link
            .spans
            .iter()
            .find_map(|span| Some((span, screen_row(span.line)?)))
        else {
            return;
        };
        let last_row = link
            .spans
            .iter()
            .rev()
            .find_map(|span| screen_row(span.line))
            .unwrap_or(first_row);

        let columns = (f(bounds.size.width - PAD_X * 2.0) / f(cell.width).max(1.0)) as usize;
        // 矮窗口里也得整个看得见:高过元素的部分会被裁掉,裁掉的偏偏是留着的尾巴。
        // 提示行占掉一行配额
        let max_rows = MAX_ROWS
            .min(prepared.screen_lines.saturating_sub(1))
            .saturating_sub(usize::from(footer.is_some()))
            .max(1);
        let (background, foreground) = tip_colors();
        let footer_color = if target.is_some() {
            Hsla {
                a: foreground.a * FOOTER_ALPHA,
                ..foreground
            }
        } else {
            foreground
        };
        let font = self.style.font();
        let rows = target
            .map(|target| links::wrap_link_target(target, columns, max_rows))
            .unwrap_or_default()
            .into_iter()
            .map(|row| (row, foreground))
            .chain(footer.map(|footer| (footer.to_string(), footer_color)));
        let lines: Vec<ShapedLine> = rows
            .map(|(row, color)| {
                let run = TextRun {
                    len: row.len(),
                    font: font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                window
                    .text_system()
                    .shape_line(row.into(), self.style.font_size, &[run], None)
            })
            .collect();
        let width = lines.iter().map(|line| line.width).max().unwrap_or(px(0.0));
        let tip = size(
            width + PAD_X * 2.0,
            cell.height * lines.len() as f32 + PAD_Y * 2.0,
        );
        let origin = links::link_target_origin(
            prepared.origin.x + cell.width * first.start as f32,
            prepared.origin.y + cell.height * first_row,
            prepared.origin.y + cell.height * (last_row + 1.0),
            tip,
            bounds,
        );
        window.paint_quad(
            fill(Bounds::new(origin, tip), background).corner_radii(Corners::all(px(4.0))),
        );
        for (row, line) in lines.iter().enumerate() {
            _ = line.paint(
                point(
                    origin.x + PAD_X,
                    origin.y + PAD_Y + cell.height * row as f32,
                ),
                cell.height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            );
        }
    }

    /// 滚动条的两个 quad(轨道可选 + 滑块)。
    ///
    /// 淡出是「按时间算 alpha + 没淡完就再要一帧」,不走 `with_animation` ——
    /// 那玩意要求一个稳定的 ElementId 且会持续请求帧,而滚动条**大多数时候
    /// 应该是完全静止的**,空转帧对一个终端来说太贵。
    pub(super) fn paint_scrollbar(&self, prepared: &PreparedFrame, window: &mut Window) {
        let Some(layout) = prepared.scrollbar else {
            return;
        };
        let drag = prepared.state.scrollbar.get();
        let idle = prepared
            .state
            .scrollbar_touched
            .get()
            .map(|t| t.elapsed())
            .unwrap_or(Duration::MAX);
        let active = drag.active();
        // 减弱动效下淡出补间归零(延迟不变),见 `ScrollbarStyle::gated`
        let style = self.scrollbar.gated();
        let alpha = scrollbar::alpha(&style, idle, active, layout.at_bottom());

        if alpha > 0.004 {
            if let Some(track) = self.scrollbar.track {
                window.paint_quad(
                    fill(
                        layout.track,
                        Hsla {
                            a: track.a * alpha,
                            ..track
                        },
                    )
                    .corner_radii(Corners::all(self.scrollbar.radius)),
                );
            }
            let base = if active {
                self.scrollbar.thumb_active.unwrap_or(Hsla {
                    a: self.scrollbar.active_alpha,
                    ..self.theme.foreground
                })
            } else {
                self.scrollbar.thumb.unwrap_or(Hsla {
                    a: self.scrollbar.idle_alpha,
                    ..self.theme.foreground
                })
            };
            window.paint_quad(
                fill(
                    layout.thumb,
                    Hsla {
                        a: base.a * alpha,
                        ..base
                    },
                )
                .corner_radii(Corners::all(self.scrollbar.radius)),
            );
        }

        // 淡出还没走完就再要一帧;走完了就此打住,不空转
        if scrollbar::needs_animation_frame(&style, idle, active) {
            window.request_animation_frame();
        }
    }
}
