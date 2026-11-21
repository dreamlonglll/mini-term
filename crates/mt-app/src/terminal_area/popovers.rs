use super::*;

/// tab 栏插入指示线的宽度。原版最初是 2px 细线,评审实测「肉眼难辨」后
/// 换成 **3px 圆头 + accent 双层光晕**(`0 0 6px` + `0 0 2px`),这里一步到位
/// 抄加强版,不复刻中间那一版。
pub(super) const TAB_DROP_LINE_W: f32 = 3.0;

/// 浮层宽度。原版是 `min-w-[280px]` + 内容撑开,gpui 侧要给正文列一个确定宽度
/// 才truncate得动,取固定值 —— 差别只在「超长正文时原版会更宽」。
///
/// ⚠️ 下面这三个尺寸都是**13px 基准下的值**,用的时候一律过
/// [`ui::font_px`] 换算 —— 原版是 `rem`,跟着根字号走;这边写死 `px` 的话,
/// 用户把 `uiFontSize` 调大之后列就装不下内容了(时间列 `15:29` 会折成两行)。
pub(super) const MARKER_PANEL_WIDTH: f32 = 300.0;
/// 列表最大高度(`MarkerList.tsx:30` 的 `max-h-80` = 20rem)。
pub(super) const MARKER_PANEL_MAX_HEIGHT: f32 = 320.0;
/// 正文截断字数(`MarkerList.tsx:16` 的 `truncate(s, 40)`)。
pub(super) const MARKER_LINE_MAX: usize = 40;
/// `#seq` 列宽(`MarkerList.tsx:41` 的 `w-8`)。
pub(super) const MARKER_SEQ_W: f32 = 32.0;
/// 时间列宽(`MarkerList.tsx:42` 的 `w-10`)。**别再往下调**:`HH:mm` 是五个
/// 字符,收窄到装不下就会折行 —— 两列都用 `min_w` 而不是 `w`,宽度算漏时
/// 宁可把列撑开也别把字折了。
pub(super) const MARKER_TIME_W: f32 = 40.0;
/// 标记浮层还该开着吗:那个 pane 得还在布局里、pty 没换,而且**仍是所在叶子的
/// 激活 tab**。
///
/// 对应原版 `PaneGroup.tsx:306-308` 的
/// `useEffect(() => setMarkerOpen(false), [activePane?.ptyId])` —— 切 tab、
/// 关 pane、分屏切换都靠它收场(浮层里那份列表是**激活 pane** 的,换了人还开着
/// 就是在看别人的标记)。
pub(super) fn marker_popover_alive(layout: &SplitNode, pane_id: &str, pty_id: u32) -> bool {
    let Some(SplitNode::Leaf {
        panes,
        active_pane_id,
        ..
    }) = layout.leaf_of_pane(pane_id)
    else {
        return false;
    };
    // 激活 tab 的解析与 render_leaf 同口径:找不到就退回第一个
    let active = panes
        .iter()
        .find(|p| &p.id == active_pane_id)
        .or_else(|| panes.first());
    active.is_some_and(|p| p.id == pane_id && p.pty_id == Some(pty_id))
}

impl TerminalArea {
    /// 开 / 关标记浮层(按钮是 **toggle**,与 Ctrl+F 的「只开不关」不同)。
    pub(super) fn toggle_marker_popover(
        &mut self,
        pane_id: &str,
        pty_id: u32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marker_open.is_some() {
            self.close_marker_popover(window, cx);
            return;
        }
        if !overlay::push(overlay::key(overlay::kind::MARKER_LIST)) {
            return;
        }
        // 开之前收拾一遍:还挂着的条目趁这一下补锚(AI 刚把排队的那条处理掉的话,
        // 它就从「灰的、点不动」变回可跳)。放在这里而不是渲染里 —— 回扫要读
        // scrollback,不能每帧跑,见 `store::refresh_markers`
        self.store
            .update(cx, |store, cx| store.refresh_markers_for_pty(pty_id, cx));
        let target = (pane_id.to_string(), pty_id);
        // 收起动画播到一半又在同一个 pane 上打开:从当前进度往回长;换了 pane 就从头长
        if self.marker_shown.as_ref() != Some(&target) {
            self.marker_reveal.hide_now();
        }
        self.marker_open = Some(target.clone());
        self.marker_shown = Some(target);
        self.marker_prev_focus = window.focused(cx);
        window.focus(&self.marker_focus, cx);
        self.marker_reveal.show(cx);
        cx.notify();
    }

    /// 收起浮层(幂等),播收起动画(播完才卸,见 [`Self::render_marker_popover`])。
    ///
    /// 焦点**一开始就**还给打开浮层前的那个元素(多半是终端):正在收的浮层不再接按键,
    /// 覆盖物登记也当场摘掉。
    pub(super) fn close_marker_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.marker_open.take().is_none() {
            return;
        }
        overlay::pop(overlay::key(overlay::kind::MARKER_LIST));
        if let Some(prev) = self.marker_prev_focus.take() {
            window.focus(&prev, cx);
        }
        self.marker_reveal.hide(cx);
        cx.notify();
    }

    /// 直接卸掉标记浮层,不播动画(它挂的 pane / pty 已经没了,或者整个项目切走了)。
    /// 与 [`Self::drop_command_popover`] 同口径。
    pub(super) fn drop_marker_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_marker_popover(window, cx);
        self.marker_reveal.hide_now();
        self.marker_shown = None;
    }

    /// 「⚑ N」按钮上的探针报来按钮窗口坐标(`pane_id` = 该叶子的 active pane)。
    /// 返回「变了、且浮层正画在这个 pane 上」—— 只有那时才要再画一帧。
    pub(super) fn note_marker_anchor(&mut self, pane_id: &str, bounds: Bounds<Pixels>) -> bool {
        if self.marker_anchors.get(pane_id) == Some(&bounds) {
            return false;
        }
        self.marker_anchors.insert(pane_id.to_string(), bounds);
        self.marker_shown
            .as_ref()
            .is_some_and(|(pane, _)| pane == pane_id)
            && self.marker_reveal.is_mounted()
    }

    // ─── 命令库浮层(issue #81)────────────────────────────────

    /// 开 / 关命令库浮层(按钮是 toggle;Ctrl+Shift+K 也走这里)。
    ///
    /// 开的一刻把目标 pane 交给浮层:命令要写进**打开时**的那个 pane,
    /// 而不是运行那一刻的焦点 pane —— 浮层开着时焦点在它自己的搜索框上。
    pub fn toggle_command_popover(
        &mut self,
        project_id: &str,
        pane_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.cmd_open.is_some() {
            self.close_command_popover(window, cx);
            return;
        }
        // 没 pty 的 pane 写不进命令,按钮也不会画 —— 只有快捷键能走到这里
        let Some(pty_id) = self
            .store
            .read(cx)
            .project_state(project_id)
            .and_then(|s| s.pane(pane_id))
            .and_then(|p| p.pty_id)
        else {
            return;
        };
        if !overlay::push(overlay::key(overlay::kind::COMMAND_LIBRARY)) {
            return;
        }
        let popover = match self.cmd_popover.clone() {
            Some(p) => p,
            None => {
                let store = self.store.clone();
                let popover = cx.new(|cx| CommandPopover::new(store, window, cx));
                self._cmd_sub =
                    Some(cx.subscribe_in(&popover, window, Self::on_command_popover_event));
                self.cmd_popover = Some(popover.clone());
                popover
            }
        };
        let target = (project_id.to_string(), pane_id.to_string(), pty_id);
        // 收起动画播到一半又在同一个 pane 上打开:从当前进度往回长;换了 pane 就从头长
        if self.cmd_shown.as_ref() != Some(&target) {
            self.cmd_reveal.hide_now();
        }
        self.cmd_open = Some(target.clone());
        self.cmd_shown = Some(target);
        self.cmd_prev_focus = window.focused(cx);
        let (pid, pane) = (project_id.to_string(), pane_id.to_string());
        popover.update(cx, |popover, cx| popover.open_for(pid, pane, window, cx));
        self.cmd_reveal.show(cx);
        cx.notify();
    }

    /// 收起命令库浮层(幂等),播收起动画(播完才卸,见 [`Self::render_command_popover`])。
    ///
    /// 焦点**一开始就**还回去:正在收的浮层不再接按键,覆盖物登记也当场摘掉。
    pub fn close_command_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.cmd_open.take().is_none() {
            return;
        }
        overlay::pop(overlay::key(overlay::kind::COMMAND_LIBRARY));
        if let Some(prev) = self.cmd_prev_focus.take() {
            window.focus(&prev, cx);
        }
        self.cmd_reveal.hide(cx);
        cx.notify();
    }

    /// 直接卸掉命令库浮层,不播动画(它挂的 pane / pty 已经没了,或者整个项目切走了)。
    pub(super) fn drop_command_popover(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.close_command_popover(window, cx);
        self.cmd_reveal.hide_now();
        self.cmd_shown = None;
    }

    /// 「命令库」按钮上的探针报来按钮窗口坐标(`pane_id` = 该叶子的 active pane)。
    /// 返回「变了、且浮层正画在这个 pane 上」—— 只有那时才要再画一帧。
    pub(super) fn note_cmd_anchor(&mut self, pane_id: &str, bounds: Bounds<Pixels>) -> bool {
        if self.cmd_anchors.get(pane_id) == Some(&bounds) {
            return false;
        }
        self.cmd_anchors.insert(pane_id.to_string(), bounds);
        self.cmd_shown
            .as_ref()
            .is_some_and(|(_, pane, _)| pane == pane_id)
            && self.cmd_reveal.is_mounted()
    }

    /// 浮层要开弹窗时**先关浮层再开**:顺序反了,Dialog 抢到的焦点会被
    /// [`Self::close_command_popover`] 的焦点还原覆盖掉(键落进终端而不是表单)。
    ///
    /// 「再开」还要**等浮层从屏幕上消失**:弹窗的毛玻璃背板抓的是 DWM 的上一帧
    /// (`main.rs` 的 frost 段),同一轮里开的话浮层还没从屏幕上消失,会被一起
    /// 糊进背景 —— 真机截到过一张带浮层残影的编辑弹窗。所以等收起动画播完,再等 50ms
    /// (够两帧,肉眼无感)。
    pub(super) fn on_command_popover_event(
        &mut self,
        _popover: &Entity<CommandPopover>,
        event: &PopoverEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_command_popover(window, cx);
        if matches!(event, PopoverEvent::Close) {
            return;
        }
        let store = self.store.clone();
        let event = event.clone();
        // 浮层要先从屏幕上消失:收起动画播完再等那两帧(毛玻璃背板抓的是上一帧)
        let wait = std::time::Duration::from_millis(50) + self.cmd_reveal.hide_remaining();
        cx.spawn_in(window, async move |_this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = cx.update(|window, cx| match event {
                PopoverEvent::Close => {}
                PopoverEvent::Edit { existing, group } => {
                    command_library::open_editor(store, existing, group, window, cx);
                }
                PopoverEvent::NewGroup => command_library::prompt_new_group(store, window, cx),
                PopoverEvent::RenameGroup(name) => {
                    command_library::prompt_rename_group(store, name, window, cx);
                }
            });
        })
        .detach();
    }

    /// 命令库浮层。层级与锚定套路照 [`Self::render_marker_popover`]:
    /// `deferred(priority 1)` → 全窗透明遮罩(按下即关)→ `anchored(按钮下缘)`
    /// → 面板实体。
    ///
    /// 开合动画走 [`crate::popover_reveal`]:框体(底色 / 边框 / 阴影)由这里按插值矩形画,面板本体
    /// 只画内容([`CommandPopover`] 不再自带框体)。收起动画里遮罩不画(点哪儿都不再被它拦),
    /// 正在收的面板也不接鼠标;播完才卸。
    pub(super) fn render_command_popover(
        &mut self,
        layout: &SplitNode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (_, pane_id, pty_id) = self.cmd_shown.clone()?;
        if !marker_popover_alive(layout, &pane_id, pty_id) {
            // 切 tab / 关 pane / pty 换了:浮层挂的地方没了,直接卸,不播动画
            self.drop_command_popover(window, cx);
            return None;
        }
        let Some(mut state) = self.cmd_reveal.frame(window) else {
            // 收起动画播完了
            self.cmd_shown = None;
            return None;
        };
        state.origin_x = self
            .cmd_anchors
            .get(&pane_id)
            .map(|anchor| anchor.left() + anchor.size.width / 2.0);
        let open = self.cmd_open.is_some();
        let popover = self.cmd_popover.clone()?;
        let rect = self.pane_rects.get(&pane_id)?;
        let inset = cmd_anchor_inset(matches!(layout, SplitNode::Split { .. }));
        let panel_width = ui::font_px(command_library::popover::PANEL_WIDTH);
        let anchor = point(
            px(rect.left + rect.width - inset - f32::from(panel_width)),
            px(rect.top + 2.0),
        );

        let panel = reveal_frame(
            popover,
            state,
            Some(Chrome {
                // 浮层用 `bg_overlay`(主题包下恒不透明),不用 `bg_elevated` —— 后者会乘
                // `surface_opacity`,面板叠在终端上时底下的字会透出来。与标题栏项目下拉同口径
                bg: ui::bg_overlay(),
                hover_bg: None,
                border_color: ui::border_subtle(),
                border_width: px(1.0),
                radius: px(6.0).into(),
                shadow: ShadowMode::Always,
                // 面板内的按下不算「点外」—— 遮罩的 on_mouse_down 靠 hitbox 判定
                occlude: true,
            }),
        );

        let size = window.viewport_size();
        Some(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .w(size.width)
                        .h(size.height)
                        // 收起动画里不拦:点哪儿都直接落到下面
                        .when(open, |el| {
                            el.occlude()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                                        this.close_command_popover(window, cx);
                                    }),
                                )
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                                        this.close_command_popover(window, cx);
                                    }),
                                )
                        })
                        .child(
                            anchored()
                                .position(anchor)
                                .snap_to_window_with_margin(px(4.0))
                                .child(panel),
                        ),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
    /// 不画、面板不接鼠标,播完才卸。
    pub(super) fn render_marker_popover(
        &mut self,
        layout: &SplitNode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (pane_id, pty_id) = self.marker_shown.clone()?;
        // 切 tab / 关 pane / 分屏切换 → 无条件关(`PaneGroup.tsx:306-308`):
        // 浮层挂的地方没了,直接卸,不播动画
        if !marker_popover_alive(layout, &pane_id, pty_id) {
            self.drop_marker_popover(window, cx);
            return None;
        }
        let Some(mut state) = self.marker_reveal.frame(window) else {
            // 收起动画播完了
            self.marker_shown = None;
            return None;
        };
        state.origin_x = self
            .marker_anchors
            .get(&pane_id)
            .map(|anchor| anchor.left() + anchor.size.width / 2.0);
        let open = self.marker_open.is_some();
        // 按钮的位置由 pane 矩形反推:pane body 的上缘就是 tab 栏下缘,
        // 右缘就是叶子右缘(见 MARKER_ANCHOR_INSET 的说明)
        let rect = self.pane_rects.get(&pane_id)?;
        // 最大化钮在场与否会让簇宽差一颗 —— 判据与 `render_leaf` 那边同一条
        // (真分了屏才画;最大化态本身就以「分了屏」为前提)
        let inset = marker_anchor_inset(matches!(layout, SplitNode::Split { .. }))
            + ssh_anchor_extra(self.ssh_button_state(pty_id, cx).map(|s| s.count));
        // 面板宽度跟着界面字号缩放,锚点必须用**换算之后**的值,否则字号一调大
        // 面板就从按钮右缘溢出去了
        let panel_width = ui::font_px(MARKER_PANEL_WIDTH);
        let anchor = point(
            px(rect.left + rect.width - inset - f32::from(panel_width)),
            // 原版是「按钮下缘 + 4」;按钮在 26px 的 tab 栏里居中,下缘约在栏底上方 2px
            px(rect.top + 2.0),
        );

        // 只画验明正身的那些 —— 与 `⚑ N` 的计数同一个口,否则会出现
        // 「按钮写着 5 条、点开只有 3 条」
        let markers = self.store.read(cx).visible_markers_for_pty(pty_id);
        // 列表本体单独一层:`overflow_y_scroll` 要 Stateful(必须带 id),
        // 而外层要 `track_focus`(Esc 的落点),两件事分层最省心
        let mut list = div()
            .id(SharedString::from(format!("marker-list-{pty_id}")))
            .w_full()
            .max_h(ui::font_px(MARKER_PANEL_MAX_HEIGHT))
            .overflow_y_scroll();

        if markers.is_empty() {
            // 到不了(按钮在 count == 0 时就不画了),照抄空态兜底 `MarkerList.tsx:22-28`
            list = list.child(
                div()
                    .px(px(12.0))
                    .py(px(8.0))
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("markerList", "empty")),
            );
        } else {
            list = list.py(px(4.0));
            for (idx, marker) in markers.into_iter().enumerate() {
                let marker_id = marker.id.clone();
                let store = self.store.clone();
                // 还没定位的条目:那条消息还在 AI 的队列里没上屏,没有行可跳。
                // **照样进列表**(用户看得到自己追加过什么),只是画成灰的,
                // 见 `markers::MarkerAnchor::Pending`
                let pending = marker.anchor.is_pending();
                list = list.child(
                    div()
                        .id(SharedString::from(format!("marker-{}", marker.id)))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(12.0))
                        .py(px(6.0))
                        .cursor_pointer()
                        .text_size(ui::font_px(12.0))
                        .text_color(if pending {
                            ui::text_muted()
                        } else {
                            ui::text_primary()
                        })
                        // `--bg-hover` 在 ui::Palette 里没有对应项。面板底色就是
                        // `bg_overlay`,悬停再刷它等于没刷,改用 `border_subtle`(同标题栏下拉)
                        .hover(|el| el.bg(ui::border_subtle()))
                        // 悬停看全文(含粘贴多行时的换行);挂着的再补一句为什么跳不了
                        .tip(if pending {
                            SharedString::from(format!(
                                "{}\n\n{}",
                                marker.line,
                                t("markerList", "pendingAnchor")
                            ))
                        } else {
                            SharedString::from(marker.line.clone())
                        })
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            cx.stop_propagation();
                            let id = marker_id.clone();
                            // 挂着的条目点一下也有意义:`jump_to_marker` 会先补一次锚,
                            // AI 刚把它处理掉的话这一下就跳过去了
                            let jumped =
                                store.update(cx, |store, cx| store.jump_to_marker(pty_id, &id, cx));
                            // 跳转**并关闭浮层**(`MarkerList.tsx:36-39`);跳不动就不关 ——
                            // 关掉的话「点了没反应」会让人以为是坏的
                            if jumped {
                                this.close_marker_popover(window, cx);
                            }
                        }))
                        // 两列都是 min_w + nowrap:宽度跟着界面字号缩放,万一还是
                        // 算窄了(字族不同、`#100` 这种三位数),让它把列撑开,
                        // 而不是把 `15:29` 折成两行
                        .child(
                            div()
                                .flex_none()
                                .min_w(ui::font_px(MARKER_SEQ_W))
                                .whitespace_nowrap()
                                .text_color(ui::text_muted())
                                // 按**可见列表**里的位置编号,不用 `marker.seq`
                                // (那是全量列表里的位置,过滤掉候选条目之后会跳号:
                                // #1、#3、#4)。原版 seq 的语义本就是「它在列表里
                                // 排第几」,这里的「列表」就是用户看到的这一份
                                .child(format!("#{}", idx + 1)),
                        )
                        .child(
                            div()
                                .flex_none()
                                .min_w(ui::font_px(MARKER_TIME_W))
                                .whitespace_nowrap()
                                .text_color(ui::text_muted())
                                .child(markers::format_time(marker.ts)),
                        )
                        // 原版是 CSS `truncate`(= hidden + ellipsis + nowrap):
                        // 40 字是**字数**上限,窄面板里照样宽过一行,不 nowrap 会折
                        .child(
                            div()
                                .flex_1()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .child(markers::truncate_line(&marker.line, MARKER_LINE_MAX)),
                        )
                        // 进行中圆点。⚠️ 最后一条**永远**亮着 —— 原版没有任何地方
                        // 在 AI 完成时把它翻掉,照抄(见 markers::AiMarker::in_progress)
                        .when(marker.in_progress, |el| {
                            el.child(
                                div()
                                    .id(SharedString::from(format!("marker-dot-{}", marker.id)))
                                    .flex_none()
                                    .w(px(6.0))
                                    .h(px(6.0))
                                    .rounded_full()
                                    .bg(ui::color_ai_working())
                                    // 原版是 aria-label,gpui 没有 aria,落成 tooltip
                                    .tip(t("markerList", "inProgress")),
                            )
                        }),
                );
            }
        }

        // 框体(圆角 / 边框 / 底色 / 阴影 / 拦鼠标)交给 reveal_frame 的 Chrome 按插值矩形画,
        // 面板本体只留内容;边框位用透明边占住,内容区与原先逐像素同位
        let panel = div()
            .track_focus(&self.marker_focus)
            .key_context("MarkerList")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    cx.stop_propagation();
                    this.close_marker_popover(window, cx);
                }
            }))
            .w(panel_width)
            .border_1()
            .border_color(gpui::transparent_black())
            .child(list);
        let panel = reveal_frame(
            panel,
            state,
            Some(Chrome {
                // 浮层用 `bg_overlay`(主题包下恒不透明),不用 `bg_elevated` —— 后者会乘
                // `surface_opacity`,面板叠在终端上时底下的字会透出来。与标题栏项目下拉同口径
                bg: ui::bg_overlay(),
                hover_bg: None,
                border_color: ui::border_subtle(),
                border_width: px(1.0),
                radius: px(6.0).into(),
                shadow: ShadowMode::Always,
                // 面板内的按下不算「点外」—— 遮罩的 on_mouse_down 靠 hitbox 判定
                occlude: true,
            }),
        );

        let size = window.viewport_size();
        Some(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .w(size.width)
                        .h(size.height)
                        // 点浮层外任意处关闭(原版挂 document 的 mousedown)。
                        // occlude 让这一层吃掉这次按下 —— 否则关浮层那一下会顺手
                        // 点到底下的终端/tab。收起动画里不拦:点哪儿都直接落到下面
                        .when(open, |el| {
                            el.occlude()
                                .on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                                        this.close_marker_popover(window, cx);
                                    }),
                                )
                                .on_mouse_down(
                                    MouseButton::Right,
                                    cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                                        this.close_marker_popover(window, cx);
                                    }),
                                )
                        })
                        .child(
                            anchored()
                                .position(anchor)
                                .snap_to_window_with_margin(px(4.0))
                                .child(panel),
                        ),
                ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }
}
