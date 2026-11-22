use super::*;

/// 控件簇里 marker 按钮**右缘**到叶子右边缘的距离(最大化钮不在场时)。
///
/// 簇是 `.gap(2).px(6)` 后跟五个 22×22 的方钮(终端内查找 / 命令库 / 分屏右 /
/// 分屏下 / 关整组,前四颗与原版 `PaneGroup.tsx:489-541` 同序,命令库钮是
/// issue #81 加的),marker 按钮排在它们之前,自己还带 4px 右外边距(原版的
/// `mr-1`)。查找、命令库与 marker 三颗同以「pane 有 pty」为前提 —— 凡是
/// marker 浮层要用这个锚点的场景五钮必然齐。
/// 原版是 `getBoundingClientRect()` 量出来的,这里由布局常量算 ——
/// 加减控件时**必须同步改这个常量**,有单测钉着组成。
///
/// marker 与查找之间还可能夹一颗「SSH 交互」按钮(该 pane 有记录才画),它的宽度随条数的位数变,
/// 不进这个常量,由 [`ssh_anchor_extra`] 另算、在 `render_marker_popover` 里加上。
pub(super) const MARKER_ANCHOR_INSET: f32 =
    CTRL_CLUSTER_PAD + 5.0 * (CTRL_BTN + CTRL_GAP) + MARKER_BTN_MARGIN_RIGHT;
pub(super) const CTRL_CLUSTER_PAD: f32 = 6.0;
pub(super) const CTRL_BTN: f32 = 22.0;
pub(super) const CTRL_GAP: f32 = 2.0;
pub(super) const MARKER_BTN_MARGIN_RIGHT: f32 = 4.0;

/// 命令库钮**右缘**到叶子右边缘的距离(最大化钮不在场时):它右边还有
/// 分屏右 / 分屏下 / 关整组三颗。浮层的右缘对齐这颗钮的右缘。
pub(super) const CMD_ANCHOR_INSET: f32 = CTRL_CLUSTER_PAD + 3.0 * (CTRL_BTN + CTRL_GAP);

/// marker 锚点的实际内缩。**最大化钮是条件出现的**(只有真分了屏才画,
/// `PaneGroup.tsx:686` 的 `layoutIsSplit || isMaximized`),在场时簇里就是六个
/// 方钮而不是五个,锚点要多让出一颗的宽度 —— 原版量 DOM 天然不会错,这边
/// 靠常量算就必须显式分档。
pub(super) fn marker_anchor_inset(has_maximize: bool) -> f32 {
    if has_maximize {
        MARKER_ANCHOR_INSET + CTRL_BTN + CTRL_GAP
    } else {
        MARKER_ANCHOR_INSET
    }
}

/// 命令库钮锚点的实际内缩,分档理由同 [`marker_anchor_inset`]。
pub(super) fn cmd_anchor_inset(has_maximize: bool) -> f32 {
    if has_maximize {
        CMD_ANCHOR_INSET + CTRL_BTN + CTRL_GAP
    } else {
        CMD_ANCHOR_INSET
    }
}

/// 控件簇里「SSH 交互」按钮的左右内边距 / 图标与数字的间距(与 marker 按钮同一套)。
pub(super) const SSH_BTN_PAD_X: f32 = 6.0;
pub(super) const SSH_BTN_GAP: f32 = 4.0;
/// 条数每一位数字占的宽(界面 12px 字号下的取值,随界面字号缩放)。
///
/// 按钮宽度**按位数定死**而不是由内容撑开:它夹在 marker 与查找之间,marker 浮层的锚点
/// 是按簇里各钮的宽度算出来的([`marker_anchor_inset`] + [`ssh_anchor_extra`]),宽度得能算。
pub(super) const SSH_BTN_DIGIT_W: f32 = 7.5;

/// SSH 按钮的宽:左右内边距 + 图标 + 间距 + 数字位数 × 每位宽。
pub(super) fn ssh_button_width(count: usize) -> f32 {
    let digits = count.max(1).ilog10() as f32 + 1.0;
    2.0 * SSH_BTN_PAD_X + CTRL_ICON + SSH_BTN_GAP + digits * f32::from(ui::font_px(SSH_BTN_DIGIT_W))
}

/// SSH 按钮在场(该 pane 有记录)时 marker 锚点要多让出的宽度:按钮宽 + 一个 gap。
/// 不在场为 0 —— 没有记录时簇里根本不画它,也不留空隙。
pub(super) fn ssh_anchor_extra(count: Option<usize>) -> f32 {
    count.map_or(0.0, |count| ssh_button_width(count) + CTRL_GAP)
}

/// 标题栏控件簇里的「SSH 交互」按钮:SSH 图标 + 条数,对应该组 active pane 的记录。
///
/// 外观与这一簇统一:盒子照 marker 按钮(`px 6` / 圆角 3 / hover `border_subtle` 底),内容照
/// `ctrl_icon`(常驻 0.6 透明度、hover 全亮并转 accent);悬浮框开着时常亮(同命令库钮的
/// `cmd_open_here`)。透明度只压在图标与数字那一层,运行中 / 失败两颗小圆点不跟着变淡。
///
/// 开合与「点完把键盘焦点还给该 pane 的终端」都在 [`SshActivityView::toggle`] 里;悬浮框本身
/// 仍画在 pane 内容区右上角(见 `ssh_activity` 模块注释)。
///
/// 按钮身上挂一个探针(不画东西、不插 hitbox 的 canvas),把按钮的窗口坐标报给视图 ——
/// 悬浮框的开合动画以它的水平中心为原点。只有坐标变了、且悬浮框开着或在动画中时才给
/// **那个视图**要一帧(下一帧直接 notify 它):pane 套着 view 级缓存,标脏 TerminalArea
/// 重画不到悬浮框,而按钮平时挪位也不该惊动任何人。
pub(super) fn ssh_activity_button(
    leaf_id: &str,
    view: Entity<SshActivityView>,
    state: ButtonState,
) -> gpui::Stateful<gpui::Div> {
    let probe = anchor_probe(view.clone(), |view, bounds| view.note_anchor_bounds(bounds));
    let group = SharedString::from(format!("ssh-activity-btn-{leaf_id}"));
    let dot = (state.running || state.failed).then(|| {
        div()
            .absolute()
            .top(px(1.0))
            .right(px(1.0))
            .w(px(6.0))
            .h(px(6.0))
            .rounded_full()
            .bg(if state.running {
                ui::accent()
            } else {
                ui::color_error()
            })
    });
    div()
        .id(SharedString::from(format!("ssh-activity-{leaf_id}")))
        .group(group.clone())
        .relative()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .w(px(ssh_button_width(state.count)))
        .h(px(CTRL_BTN))
        .rounded(px(3.0))
        .cursor_pointer()
        .when(state.open, |el| el.bg(ui::border_subtle()))
        .hover(|el| el.bg(ui::border_subtle()))
        .tip(t("sshActivity", "buttonTip"))
        .on_click(move |_: &ClickEvent, window, cx| {
            // 别冒到 tab 栏的「双击空白处最大化」上
            cx.stop_propagation();
            view.update(cx, |view, cx| view.toggle(window, cx));
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(SSH_BTN_GAP))
                .text_color(ui::text_muted())
                .opacity(if state.open { 1.0 } else { 0.6 })
                .group_hover(group, |el| el.opacity(1.0).text_color(ui::accent()))
                .child(
                    VectorIcon::new(crate::activity_bar::SSH, px(CTRL_ICON)).ink(ui::text_muted()),
                )
                .child(state.count.to_string()),
        )
        .children(dot)
        .child(probe)
}

/// tab 栏高度 —— 折叠标题条与它等高(折叠条就是「只剩标题栏的那一格」,
/// 高度对不上会让最大化前后的视线落点跳一下)。
///
/// 26 → 30:tab 改成胶囊形后要给上下各留一点呼吸位([`TAB_CHIP_H`] 22 +
/// 上下各 4),不然胶囊顶着条的上下边缘、看不出是「浮在条上的一颗」。
pub(super) const TAB_BAR_H: f32 = 30.0;

/// 叶子 tab 胶囊的高度。条高减去上下各 4px 的留白。
pub(super) const TAB_CHIP_H: f32 = 22.0;
/// 胶囊圆角。
pub(super) const TAB_CHIP_ROUND: f32 = 6.0;
/// 胶囊最窄 / 最宽。**不再 `justify_center` + `min_w(110)`** —— 那套让
/// 「pwsh ×」缩在正中、两侧各留 30px 空白,× 也贴着文字而不靠边。
pub(super) const TAB_CHIP_MIN_W: f32 = 72.0;
pub(super) const TAB_CHIP_MAX_W: f32 = 200.0;
/// tab 上的图标槽:状态灯与 shell 图标共用这一格,**尺寸固定**,
/// 状态一变(idle ↔ ai-working)也不许让后面的标题横跳。
pub(super) const TAB_ICON_SLOT: f32 = 16.0;
/// tab 标题副段的字号(主段小一档)。
pub(super) const TAB_SECONDARY_FONT: f32 = 11.0;
/// 「做完了还没看」的绿点直径。住在 × 那一格里(见展开态 chip 的 × 槽位),
/// 比原先插在标题后面的 5px 大一号:它现在占的是关闭钮的位置,太小像误点的像素。
pub(super) const TAB_UNSEEN_DOT: f32 = 6.0;
/// 折叠标题条上绿点离 tab 右缘的距离:落在 tab 自己的右内衬里,不占文字的位置。
pub(super) const COLLAPSED_UNSEEN_INSET: f32 = 2.0;

/// tab 上的标题:主段 + 可选副段(`主段 · 副段`)。
///
/// 拆成一个独立的小助手,是为了给**并行开发中的「标题跟随 shell 的 OSC 标题」**
/// 留接口:那一支会在 store 上提供 `pane_title_parts(project_id, pane) ->
/// (String, Option<String>)`,合并时只要把这里的 `secondary` 从 `None` 换成
/// 它的第二个返回值即可,tab 那一大坨渲染代码一行都不用动。
///
/// ⚠️ 外层容器是 `min_w(0) + flex_1 + overflow_hidden`,省略号由**内部两段各自**
/// 的 `truncate()` 负责 —— `truncate()` 与 `line_clamp()` **不能混用**
/// (见 `project_gpui_truncate_ellipsis` 的口径),这里一处都没有 clamp。
pub(super) fn tab_label(
    primary: SharedString,
    secondary: Option<SharedString>,
    active: bool,
) -> impl IntoElement {
    div()
        .min_w(px(0.0))
        .flex_1()
        .flex()
        .items_center()
        .overflow_hidden()
        .child(div().min_w(px(0.0)).truncate().child(primary))
        .when_some(secondary, |el, secondary| {
            el.child(
                div()
                    .min_w(px(0.0))
                    .pl(px(4.0))
                    .truncate()
                    .text_size(ui::font_px(TAB_SECONDARY_FONT))
                    // 激活 chip 上的副段略亮一点,未激活的再退半档 ——
                    // 否则一条 tab 栏上同一批灰字分不出哪条在前台
                    .text_color(ui::with_alpha(
                        ui::text_muted(),
                        if active { 1.0 } else { 0.75 },
                    ))
                    .child(format!("· {secondary}")),
            )
        })
}

/// tab 最左的图标槽:有 AI / 错误 / 待处理状态时画对应状态灯(**AI 状态优先**);
/// 否则若是鼠标右键「SSH 连接」已连上的终端,画蓝色「已连接」灯(`◉`);
/// 再否则画 shell 图标。
///
/// 全部落进同一个固定尺寸的盒子 —— 状态一变(shell ↔ 已连接 ↔ ai-working ↔ 等你处理)
/// 标题绝不横跳。展开态 chip 与折叠标题条共用。
///
/// `contrast` 给挖空语义的笔画(fish 的眼睛)用,传图标所在容器的底色。
pub(super) fn tab_icon_slot(
    pane_id: &str,
    light: StatusLight,
    ssh_connected: Option<&str>,
    shell_name: &str,
    is_active: bool,
    contrast: Hsla,
) -> impl IntoElement {
    let show_ssh = light == StatusLight::Idle && ssh_connected.is_some();
    div()
        .id(gpui::SharedString::from(format!("tab-icon-{pane_id}")))
        .w(px(TAB_ICON_SLOT))
        .h(px(TAB_ICON_SLOT))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        // 未激活 tab 上的图标压暗一档,与弱化的标题同调。「等你处理」不压:
        // 它恰恰是给后台页签用的信号,压暗的叹号会埋进一排灰色 tab 里
        .when(!is_active && light != StatusLight::Attention, |el| {
            el.opacity(0.7)
        })
        .when_some(
            show_ssh.then_some(ssh_connected).flatten(),
            |el, conn_name| {
                el.tip(tr!(
                    "panels",
                    "statusDot.sshConnectedWith",
                    name = conn_name.to_string()
                ))
            },
        )
        .child(if light != StatusLight::Idle {
            // 动画相位来自进程级墙钟,不带逐元素状态,所以不需要 id
            ui::status_dot(light).into_any_element()
        } else if show_ssh {
            ui::ssh_connected_dot().into_any_element()
        } else {
            ShellIcon::new(ShellKind::from_shell_name(shell_name))
                .size(px(14.0))
                // 「未知」那枚跟随主题色;其余八家写死品牌色
                .color(if is_active {
                    ui::text_primary()
                } else {
                    ui::text_muted()
                })
                .contrast(contrast)
                .into_any_element()
        })
}

impl TerminalArea {
    /// 某个 pane 的「SSH 交互」按钮状态;没有记录(或终端不在了)为 `None`。
    pub(super) fn ssh_button_state(&self, pty_id: u32, cx: &App) -> Option<ButtonState> {
        let pane = self.store.read(cx).terminal(pty_id)?;
        pane.read(cx).ssh_activity().read(cx).button_state()
    }

    /// AI 任务标记浮层。`None` = 没开 / 那个 pane 已经不在了。
    ///
    /// 层级照 `menu.rs` 的套路:`deferred(priority 1)` → 全窗透明遮罩(`occlude` +
    /// 按下即关)→ `anchored(按钮下缘).snap_to_window_with_margin(4px)` → 面板。
    /// **不复用 `menu::show`**:`MenuItem` 只有 label/shortcut/danger/disabled/submenu
    /// 五种表达,装不下「#seq + 时间 + 正文 + 进行中圆点」四栏。
    ///
    /// 开合动画照命令库浮层那套([`crate::popover_reveal`]):原点是「⚑ N」按钮的水平中心,
    /// 框体(底色 / 边框 / 阴影)由这里按插值矩形画,面板本体只画内容;收起动画里遮罩
    /// 一个叶子的 tab 栏本体:每个 pane 一颗 tab(含拖起 / 合并 / 重排的拖拽
    /// 挂载)+ 末尾那颗新建终端的 `+`。右侧控件簇与落点层分别在
    /// [`Self::render_leaf_controls`] 与 [`Self::render_leaf_drop_layer`]。
    pub(super) fn render_leaf_tab_bar(
        &mut self,
        panes: &[PaneState],
        active: &PaneState,
        project_id: &str,
        leaf_id: &str,
        dragging: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        // 借的是 `*cx` 不是 `self`,所以下面补焦点句柄那段照样能改 self
        let store = self.store.read(cx);
        let unseen: Vec<bool> = panes
            .iter()
            .map(|p| store.is_pane_unseen_done(&p.id))
            .collect();
        // tab 上的 AI 品牌图标:显示条件与 agent 取值都照抄原版(见 PaneState 上
        // 的两个方法);`aiAutoResume` 缺省开启,与 store 里那处取值同口径
        let auto_resume = store.config().ai_auto_resume.unwrap_or(true);
        let vendors: Vec<Option<AiVendor>> = panes
            .iter()
            .map(|p| {
                if !p.shows_ai_session(auto_resume) {
                    return None;
                }
                let agent = p.ai_agent()?;
                // CLI 名直取(claude/codex/grok),其余 CLI(opencode/pi/gemini…)
                // 走与前端 `inferVendor` 同规则同优先级的词匹配 —— 原版 tab 上
                // 调的就是 `inferVendor({ agent })`,只认三家会漏掉它们的图标
                AiVendor::from_session_type(agent).or_else(|| AiVendor::infer(Some(agent), None))
            })
            .collect();

        // 页签区横向滚动(E.2):tab **不压缩**(`min_w` 之下就溢出),溢出时页签区
        // 自己横向滚。滚动区**只装页签**:「+」与右侧控件簇在它外面、固定在右边 ——
        // 此前整条 tab 栏一起滚,页签一多「+」与查找 / 分屏 / 关组几颗钮都被滚到视野外。
        // 页签区 `min_w(0)` + 可收缩:页签少时按内容宽,「+」紧跟在最后一个页签后面;
        // 放不下时收窄成滚动区,「+」停在右侧。
        //
        // **垂直滚轮不必自己映射**:gpui 只在 `overflow.x == Scroll && overflow.y != Scroll`
        // 且 `restrict_scroll_to_axis == false`(默认)时把 `delta.y` 记到 x 上
        // (gpui-0.2.2 `elements/div.rs:2422-2428`,默认值见 `style.rs:741`)——
        // 与原版靠 WebView 免费拿到的那条行为等价。
        //
        // 激活页签变了(切 tab / Ctrl+1..9 / 跳待办 / 新建落在末尾 / 左右移)时把它滚进
        // 视野,**只滚这一次**:用户自己横向滚过的位置不会被拽回去(见 [`TabFollow`])。
        // 不用上游的 Scrollbar 组件:实测它的轨道会吞页签下缘的点击(定稿结论)。
        let active_idx = panes.iter().position(|p| p.id == active.id).unwrap_or(0);
        let strip_scroll = self.tab_strips.entry(leaf_id.to_string()).or_default();
        let laid_out = strip_scroll.handle.bounds().size.width > px(0.0);
        if strip_scroll
            .follow
            .observe(&active.id, active_idx, laid_out)
        {
            strip_scroll.handle.scroll_to_item(active_idx);
        }
        let scroll_handle = strip_scroll.handle.clone();
        // 这组页签的顺序比上一帧变了(新建 / 关闭 / 拖拽落位 / 左右移):这一帧被挤动的
        // chip 起位移补间,新出现的原地展开(见 [`fx`])。拖拽进行中布局不动,只在落位
        // 那一刻起补间
        let reflow = self.fx.borrow_mut().arm_tab_strip(leaf_id, panes);
        let mut strip = div()
            .id(gpui::SharedString::from(format!("tabstrip-{leaf_id}")))
            .flex()
            .items_center()
            .gap(px(4.0))
            .h_full()
            .min_w(px(0.0))
            .overflow_x_scroll()
            .track_scroll(&scroll_handle);

        // tab 焦点句柄先补齐(下面的循环里 `cx` 要交给 listener,腾不出可变借用)
        for pane in panes.iter() {
            if !self.tab_focus.contains_key(&pane.id) {
                let handle = cx.focus_handle();
                self.tab_focus.insert(pane.id.clone(), handle);
            }
        }

        for (idx, pane) in panes.iter().enumerate() {
            let is_active = pane.id == active.id;
            let pane_id = pane.id.clone();
            let pane_id_key = pane.id.clone();
            let pane_id_hover = pane.id.clone();
            let pid_key = project_id.to_string();
            let tab_focus = self.tab_focus.get(&pane.id).cloned();
            let pane_id_rename = pane.id.clone();
            let pid_click = project_id.to_string();
            let pane_id_close = pane.id.clone();
            let pane_id_menu = pane.id.clone();
            let pane_id_middle = pane.id.clone();
            let pid_middle = project_id.to_string();
            let pid_close = project_id.to_string();
            let pid_rename = project_id.to_string();
            let pid_menu = project_id.to_string();
            // tab 标题走 store 的两段口径:主段 = 自定义名 > 远程连接名 > shell 名,
            // 副段 = shell 自报的 OSC 标题(`AppStore::pane_title_parts`)。
            // 恢复布局时远程 pane 的 shellName 会被映射成本地 shell 名、**不可信**,
            // 所以远程那一档必须由 store 查连接表补上(`remoteProject.ts::paneDisplayLabel`)
            let (primary, secondary) = store.pane_title_parts(project_id, pane);
            // 右键菜单的「重命名」与双击改名只预填**主段**:拿拼好的一行去预填,
            // 用户一点重命名就会把 shell 灌进来的那半截目录当成自己的名字存下来
            let label_menu = primary.clone();
            let label_rename = primary.clone();
            // 拖影只有一行文本可用,走拼好的一行
            let label_text = store.pane_display_label(project_id, pane);
            let has_unseen = unseen.get(idx).copied().unwrap_or(false);
            let vendor = vendors.get(idx).copied().flatten();
            let this_area = cx.entity();
            let pid_drag = project_id.to_string();
            // 与 `has_active_drag` 与门:拖拽被中断(松手在窗外)时 gpui 会清
            // active_drag 并重画,变淡自动撤销,不必到处补清理
            let is_dragging_self = dragging && self.pane_drag.as_deref() == Some(pane.id.as_str());
            // 悬停态本来就由下面那个 `on_hover` 维护着(它给缩略图预览用),
            // 这里直接读:chip 的悬停底色与「× 现不现身」都看它
            let is_hovered = self.hovered_tab.as_deref() == Some(pane.id.as_str());
            let chip = div()
                .id(gpui::SharedString::from(format!("tab-{}", pane.id)))
                .relative()
                // tab 键盘可达(原版 `role="tab"` + `tabIndex`):拿到焦点后
                // Enter/Space 激活。原版是 roving tabindex(只有激活 tab 是
                // 0),这里所有 tab 都可 Tab 到 —— gpui 没有 roving 语义,
                // 而「Tab 只能到激活 tab」等于隐藏 tab 键盘不可达
                .when_some(tab_focus, |el, focus| el.track_focus(&focus).tab_index(0))
                .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                    if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                        cx.stop_propagation();
                        this.store.update(cx, |store, cx| {
                            store.activate_pane(&pid_key, &pane_id_key, window, cx)
                        });
                    }
                }))
                // 激活 tab 的画面就在眼前,只有隐藏 tab 需要预览。
                // ⚠️ 离开分支必须先核对「离开的正是我们记着的那一个」:相邻
                // 元素的 enter/leave 到达顺序不保证,直接清会把刚进来的那个
                // 抹掉(鼠标沿 tab 栏横扫时预览再也弹不出来)
                .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                    let mine = this.hovered_tab.as_deref() == Some(pane_id_hover.as_str());
                    if *hovered {
                        if mine {
                            return;
                        }
                        this.hovered_tab = Some(pane_id_hover.clone());
                        if is_active {
                            this.close_tab_preview(cx);
                        } else {
                            this.schedule_tab_preview(pane_id_hover.clone(), cx);
                        }
                    } else {
                        if !mine {
                            return;
                        }
                        this.hovered_tab = None;
                        this.close_tab_preview(cx);
                    }
                    cx.notify();
                }))
                // ── 胶囊(chip)形态 ──────────────────────────────────
                //
                // **这一层不画 accent 顶线**:顶线是「哪一页在前台」的标识,
                // 归上面那条工作台页签条独占。两层各画一条橙线时两条线上下
                // 贴着、层级根本读不出来。这一层改用「底色 + 描边」的胶囊:
                // 激活 = elevated 底 + 描边 + 主文色;未激活 = 透明底 + 弱文色;
                // 悬停未激活 = 一层极淡的底(从前鼠标扫过 chip 毫无反馈,
                // 只有 500ms 后弹出的缩略图)。
                .h(px(TAB_CHIP_H))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .px(px(8.0))
                .min_w(px(TAB_CHIP_MIN_W))
                .max_w(px(TAB_CHIP_MAX_W))
                .rounded(px(TAB_CHIP_ROUND))
                .cursor_pointer()
                // 描边**三态都占位**,只换颜色:只给激活态加 border 会让未激活
                // 的 chip 内容区少 2px,切 tab 时整条 tab 栏横跳一下
                .border_1()
                .when(is_active, |el| {
                    el.bg(ui::bg_elevated())
                        .text_color(ui::text_primary())
                        .border_color(ui::border_default())
                })
                .when(!is_active, |el| {
                    el.text_color(ui::text_muted())
                        .border_color(ui::with_alpha(ui::border_default(), 0.0))
                        .when(is_hovered, |el| {
                            el.bg(ui::with_alpha(ui::text_muted(), 0.12))
                        })
                })
                // ── tab 拖起(v0.14.0):移动 / 合并 / 重排 ─────────────
                //
                // 照 `project_list.rs` 的既有模式:记下源 id 让本行变淡 +
                // 交出一个跟着鼠标走的拖影。**起拖阈值是 gpui 内建的 2px**
                // (原版是 5px 曼哈顿),差异见 `dnd` 模块注释第 3 条。
                // 原版那套「松手后一次性抑制 click」不需要 —— gpui 拖起时
                // 会把 `clicked_state` 清掉。
                .on_drag(
                    crate::dnd::DragPane {
                        project_id: pid_drag.clone(),
                        pane_id: pane.id.clone(),
                    },
                    {
                        let this = this_area.clone();
                        let label = label_text.clone();
                        move |item: &crate::dnd::DragPane, _offset, _window, cx| {
                            let id = item.pane_id.clone();
                            this.update(cx, |area: &mut TerminalArea, _cx| {
                                area.pane_drag = Some(id);
                            });
                            crate::dnd::preview(
                                label.clone(),
                                crate::dnd::PreviewIcon::Terminal,
                                cx,
                            )
                        }
                    },
                )
                // 源 tab 变淡(原版 `el.style.opacity = '0.4'`)
                .when(is_dragging_self, |el| el.opacity(0.4))
                // 单击切 tab,双击改名(旧版是右键菜单里的「重命名」);切过来的
                // 那一下凑成的双击不改名(见 `SwitchClickGuard`)。
                // ⚠️ 必须 `stop_propagation`:tab 栏那层挂着「双击空白处最大化」,
                // 不截断的话在 tab 上双击会**同时**改名和最大化
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.close_tab_preview(cx);
                    let count = click_count(event);
                    match this.tab_click.click(&pane_id, is_active, count) {
                        SwitchClick::Switch => this.store.update(cx, |store, cx| {
                            store.activate_pane(&pid_click, &pane_id, window, cx)
                        }),
                        SwitchClick::Rename => {
                            // 先把焦点交给这个(本来就激活的)页签的终端再开改名框:
                            // tab 可聚焦(上面的 `tab_index`),双击的第二下按下时焦点
                            // 落在 tab 元素上,弹窗关掉会把焦点还给它 —— 改完名敲的字
                            // 进不了终端,要再按一下空格把 tab 激活才行。右键菜单那条
                            // 「重命名」不受影响:菜单收起时已把焦点还回终端
                            this.store.update(cx, |store, cx| {
                                store.focus_pane(&pid_click, &pane_id, window, cx)
                            });
                            let (label, store) = (label_rename.clone(), this.store.clone());
                            modal::open_rename_pane(
                                store,
                                pid_rename.clone(),
                                pane_id_rename.clone(),
                                label,
                                window,
                                cx,
                            );
                        }
                        // 前一下刚把它切过来:第二下按下时焦点又落回 tab 元素(同上),
                        // 不交回去的话切过去之后敲的字全丢。它此刻已是激活页签
                        SwitchClick::Ignore => this.store.update(cx, |store, cx| {
                            store.focus_pane(&pid_click, &pane_id, window, cx)
                        }),
                    }
                }))
                // tab 右键菜单(`PaneGroup.tsx` 的 paneContextMenu)
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        // 开菜单前先收缩略图(原版 onContextMenu 第一句)
                        this.close_tab_preview(cx);
                        let entries =
                            tab_menu(&this.store, &pid_menu, &pane_id_menu, &label_menu, cx);
                        menu::show(event.position, entries, window, cx);
                    }),
                )
                // 中键单击 = 关闭(浏览器 / IDE 的惯例,松开时关;文件页签同一手法)。
                // 与 × 一字不差:同一道连点闸、同一个 AI 感知确认入口
                .on_mouse_up(
                    MouseButton::Middle,
                    cx.listener(move |this, _event: &MouseUpEvent, window, cx| {
                        cx.stop_propagation();
                        this.close_tab_preview(cx);
                        let dead = pane_actions::pane_is_dead(
                            this.store.read(cx),
                            &pid_middle,
                            &pane_id_middle,
                        );
                        if this.swallows_close_click(dead, cx) {
                            return;
                        }
                        pane_actions::close_pane(
                            this.store.clone(),
                            pid_middle.clone(),
                            pane_id_middle.clone(),
                            window,
                            cx,
                        );
                    }),
                )
                // ── 图标槽(16×16,固定)──────────────────────────
                //
                // 闲着的时候画 **shell 图标**,有状态的时候才让位给状态灯:
                // `idle` 那颗状态灯是个空心圈,一屏五个闲置终端就是五颗
                // 一模一样的 ○,占着位置不带任何信息。
                //
                // 两个分支都塞进同一个固定尺寸的盒子 —— 状态一变(idle ↔
                // ai-working ↔ 等你处理)标题就跟着横跳一格是绝对不行的。
                //
                // 画的是**显示档位**(四态 + attention):Claude 等授权不再是绿勾、
                // Codex 等授权不再是转圈,而是第五档的叹号。
                .child(tab_icon_slot(
                    &pane.id,
                    pane.light(),
                    pane.ssh_connected.as_deref(),
                    &pane.shell_name,
                    is_active,
                    // fish 的眼睛是挖空语义,给它 chip 的底色
                    if is_active {
                        ui::bg_elevated()
                    } else {
                        ui::bg_terminal()
                    },
                ))
                // AI 品牌图标(原版 `PaneGroup.tsx` 的 `aiActive && <BrandIcon/>`):
                // 只在这个 pane 真有 AI 会话身份时出现,认不出厂商就不占位
                .when_some(vendor, |el, vendor| {
                    el.child(
                        BrandIcon::new(Some(vendor))
                            .size(px(12.0))
                            // VectorIcon 不继承 text_color,跟着 tab 的明暗自己喂
                            .color(if is_active {
                                ui::text_primary()
                            } else {
                                ui::text_muted()
                            }),
                    )
                })
                .child(tab_label(
                    primary.into(),
                    secondary.map(SharedString::from),
                    is_active,
                ))
                // ── × 槽位(14×14,常驻)────────────────────────────
                //
                // 「做完了还没看」的绿点也住在这一格(VS Code 未保存点的做法):
                // 有点就画点,鼠标进 chip 才换回 ×。此前绿点是标题后面另插的
                // 一颗,每次完成都让胶囊宽 11px、后面的页签整排挪一下;共用这一格
                // 之后绿点来去是零宽度变化。
                .child(
                    div()
                        .id(gpui::SharedString::from(format!("tab-close-{}", pane.id)))
                        .w(px(14.0))
                        .h(px(14.0))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.0))
                        .text_color(ui::text_muted())
                        .hover(|el| el.bg(ui::bg_overlay()).text_color(ui::color_error()))
                        // × **始终占位**,只是平时透明:靠 `when` 把它整个摘掉
                        // 的话,鼠标一进 chip 就多出 14+6px、标题当场被挤窄,
                        // 长标题还会在悬停的一瞬间换一次省略位置
                        .when(!has_unseen && !is_active && !is_hovered, |el| {
                            el.opacity(0.0)
                        })
                        // tab 上的 × 与右键「关闭此终端」同一个入口:关之前
                        // 盘点 AI 会话并确认(原版 `closePane` 默认 confirm)
                        .on_click(cx.listener(move |this, _event, window, cx| {
                            cx.stop_propagation();
                            let dead = pane_actions::pane_is_dead(
                                this.store.read(cx),
                                &pid_close,
                                &pane_id_close,
                            );
                            if this.swallows_close_click(dead, cx) {
                                return;
                            }
                            pane_actions::close_pane(
                                this.store.clone(),
                                pid_close.clone(),
                                pane_id_close.clone(),
                                window,
                                cx,
                            );
                        }))
                        // 悬停时一律是 ×:点下去的只会是关闭钮,不会是绿点
                        .child(if has_unseen && !is_hovered {
                            div()
                                .w(px(TAB_UNSEEN_DOT))
                                .h(px(TAB_UNSEEN_DOT))
                                .rounded_full()
                                .bg(ui::color_success())
                                .into_any_element()
                        } else {
                            "×".into_any_element()
                        }),
                );
            // 包一层位移补间(见 [`fx`]):新建 / 关闭 / 拖拽落位挤动它时从旧位置滑过来,
            // 新出现时原地裁剪展开。tab 的屏幕矩形也由这一层报 —— 报的是**排版**位置
            // (挪动之前),缩略图锚点 + tab 栏拖拽插入位都要它。**每个 tab 都报**(原先
            // 只量悬停中的那一个):插入位要算的是「指针落在哪个 tab 的中线哪一侧」,
            // 那需要**全部** tab 的横向区间。故意不 notify —— 量完再触发重画就是每帧
            // 一个死循环。
            if self.rendering_exit {
                strip = strip.child(chip);
            } else {
                let report_id = pane.id.clone();
                strip = strip.child(
                    fx::flip_slot(
                        chip,
                        self.fx.clone(),
                        format!("{leaf_id}\u{1}{}", pane.id),
                        Some(idx),
                        scroll_handle.clone(),
                        reflow,
                    )
                    .on_bounds(move |bounds, cx| {
                        this_area.update(cx, |area: &mut TerminalArea, _cx| {
                            area.tab_rects.insert(report_id.clone(), bounds);
                        });
                    }),
                );
            }
        }

        // ⚠️ 外层这条**不刷底色**(原先是 `bg_elevated()`)。
        //
        // 它属于「终端页」,而上面那条工作台页签条里激活的「终端」页签用的是
        // `bg_terminal` —— 两者同色才连成一片。此前用 elevated,于是从上往下是
        // 「激活页签 → 更亮的一条带 → 终端内容」,页签与内容之间凭空多出一层,
        // 「页签连着内容」的隐喻断在这儿。
        //
        // 要的颜色正是 `bg_terminal`,但**不能自己刷一遍**:TerminalArea 根容器
        // 已经刷过一层(见 `render` 尾部那句),背景图主题下 bg_terminal 是半透明
        // rgba,再刷就是透明度叠乘、这一条会比下面的终端内容暗一块。不刷即可
        // 透出根容器那层,颜色分毫不差 —— 与 PaneGroup 根「一律不刷底色」同一条
        // 口径(`styles.css:151` / `themePackManager.ts:294`,见下方 2540 行注释)。
        // 与终端内容之间仍留 `border_b_1`,那是这一层唯一的分界线。
        let mut bar = div()
            .id(gpui::SharedString::from(format!("tabbar-{leaf_id}")))
            .flex()
            .items_center()
            .flex_none()
            .gap(px(4.0))
            // 只给左内衬:右边缘是 `render_leaf_controls` 那一格,它自带
            // `px(CTRL_CLUSTER_PAD)`。这里再补一道右内衬会把控件簇整体左推,
            // 而 marker 浮层的锚点([`MARKER_ANCHOR_INSET`])是按「簇紧贴叶子
            // 右缘」算出来的常量,推一下就对不上了。
            .pl(px(6.0))
            .h(px(TAB_BAR_H))
            .border_b_1()
            .border_color(ui::border_subtle())
            .text_size(ui::font_px(12.0))
            .child(strip);

        // 新建终端(在滚动区外,页签再多也停在视野里)
        let pid_new = project_id.to_string();
        let anchor_new = active.id.clone();
        let new_btn = div()
            .id(gpui::SharedString::from(format!("tab-new-{leaf_id}")))
            // 与 chip 同一套手感:方形圆角 + 悬停底色(原先只是一个裸「+」,
            // 悬停只换文字色,按上去没有「这是一颗按钮」的实感)
            .w(px(TAB_CHIP_H))
            .h(px(TAB_CHIP_H))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(TAB_CHIP_ROUND))
            .cursor_pointer()
            .text_color(ui::text_muted())
            .hover(|el| {
                el.bg(ui::with_alpha(ui::text_muted(), 0.12))
                    .text_color(ui::accent())
            })
            // 左键单击**直接弹 shell 选择菜单**(不是长按、不是下拉箭头);
            // 只有一个 shell 时不弹 —— 否则单 shell 用户每次多点一下
            // (`PaneGroup.tsx:218-232` 那道 `<= 1` 的闸)
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                // 与 tab 同理:别让这一下冒到 tab 栏的「双击空白处最大化」上
                cx.stop_propagation();
                let (shells, launchers) =
                    pane_actions::new_terminal_menu_data(this.store.read(cx), &pid_new);
                if !pane_actions::should_show_new_terminal_menu(shells.len(), launchers.len()) {
                    this.store.update(cx, |store, cx| {
                        store.new_terminal(&pid_new, None, Some(anchor_new.clone()), window, cx);
                    });
                    return;
                }
                let mut entries = pane_actions::new_terminal_menu_entries(
                    shells,
                    launchers,
                    {
                        let store = this.store.clone();
                        let (pid, anchor) = (pid_new.clone(), anchor_new.clone());
                        move |shell, window, cx| {
                            let (pid, anchor) = (pid.clone(), anchor.clone());
                            store.update(cx, |store, cx| {
                                store.new_terminal(&pid, Some(shell), Some(anchor), window, cx);
                            });
                        }
                    },
                    {
                        let store = this.store.clone();
                        let (pid, anchor) = (pid_new.clone(), anchor_new.clone());
                        move |launcher, window, cx| {
                            let (pid, anchor) = (pid.clone(), anchor.clone());
                            store.update(cx, |store, cx| {
                                store.new_terminal_from_launcher(
                                    &pid,
                                    &launcher,
                                    Some(anchor),
                                    window,
                                    cx,
                                );
                            });
                        }
                    },
                );
                entries.push(menu::separator());
                entries.push(reopen_closed_menu_item(&this.store, &pid_new, cx));
                menu::show(click_position(event, window), entries, window, cx);
            }))
            .child("+");
        // 「+」钮跟着页签区的宽度挪:新建 / 关闭让它挪位时同样从旧位置滑过来(见 [`fx`]);
        // 页签多到溢出、随窗口宽度挪的不补间(那一帧没上膛)
        bar = bar.child(fx::flip_slot(
            new_btn,
            self.fx.clone(),
            format!("{leaf_id}\u{1}+"),
            None,
            scroll_handle,
            reflow,
        ));

        bar
    }

    /// tab 栏右侧的控件簇:⚑N / SSH 交互 / 查找 / 命令库 / 最大化 / 左右分屏 / 上下分屏 / 关整组。
    /// 挂在 tab 栏的 `ml_auto` 那一格里(见 [`Self::render_leaf_tab_bar`])。
    pub(super) fn render_leaf_controls(
        &mut self,
        panes: &[PaneState],
        active: &PaneState,
        project_id: &str,
        leaf_id: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let store = self.store.read(cx);
        // AI 任务标记数。**列表为空就整个不画按钮**(`PaneGroup.tsx:489`),
        // 这就是「⚑ 平时看不见」的直接原因 —— 见 `markers` 模块注释的 alt screen 段。
        let marker_count = active
            .pty_id
            .map(|id| markers::visible(store.markers_for_pty(id)).count())
            .unwrap_or(0);
        // 最大化钮的出现条件(`PaneGroup.tsx:686`):真分了屏才有意义。
        // `maximized_pane_id()` 自带「布局是 split」这道闸,所以这里一个判据够用
        let layout_is_split = store
            .project_state(project_id)
            .and_then(|s| s.active_layout())
            .is_some_and(|l| matches!(l, SplitNode::Split { .. }));
        let is_maximized = store
            .maximized_pane_id(project_id)
            .is_some_and(|id| panes.iter().any(|p| p.id == id));
        // 右侧:查找 / 分屏 / 关整组(原版 `ctrlBtn`:常驻 60% 透明度,hover 全亮
        // + `--border-subtle` 底。图标是 `PaneGroup.tsx:40-62` 那几条 SVG 的自绘
        // 搬运;VectorIcon 的 ink 定死在构造期,hover 换色进不去 —— 与
        // `activity_bar::strip_button` 同取舍,反馈靠透明度与底色变化)
        let ctrl_icon = |id: gpui::SharedString, shapes: &'static [Shape]| {
            div()
                .id(id)
                .flex()
                .items_center()
                .justify_center()
                .w(px(CTRL_BTN))
                .h(px(CTRL_BTN))
                .rounded(px(3.0))
                .cursor_pointer()
                .opacity(0.6)
                .hover(|el| el.opacity(1.0).bg(ui::border_subtle()))
                .child(VectorIcon::new(shapes, px(CTRL_ICON)).ink(ui::text_muted()))
        };
        // 终端内查找:与 marker 按钮同一道「有 pty 才画」的闸
        // (原版 `PaneGroup.tsx:504`,`activePane.ptyId !== undefined`)
        let search_btn = active.pty_id.map(|pty_id| {
            // 查找条开合动画的原点探针:报给这个 pane(查找条由它画),见 `crate::popover_reveal`
            let probe = store
                .terminal(pty_id)
                .cloned()
                .map(|pane| anchor_probe(pane, |pane, bounds| pane.note_search_anchor(bounds)));
            ctrl_icon(
                gpui::SharedString::from(format!("term-search-{leaf_id}")),
                ICON_SEARCH,
            )
            .relative()
            // 快捷键取自键位表此刻生效的那份(与设置页 / 首启引导同一份),用户改键 / 解绑
            // 跟着变,解绑了括号里写「未设置」
            .tip(tr!(
                "paneGroup",
                "searchTooltip",
                shortcut = crate::hotkeys::hotkey_tip("terminalSearch")
            ))
            .on_click(cx.listener(move |this, _event, window, cx| {
                cx.stop_propagation();
                let pane = this.store.read(cx).terminal(pty_id).cloned();
                if let Some(pane) = pane {
                    pane.update(cx, |pane, cx| pane.open_search(window, cx));
                }
            }))
            .children(probe)
        });
        // 命令库(issue #81):与查找同一道「有 pty 才画」的闸 —— 没 pty 的 pane
        // 命令写不进去。开着时钮常亮,提示「浮层就是从我这弹出来的」
        let cmd_open_here = self
            .cmd_open
            .as_ref()
            .is_some_and(|(_, pane, _)| *pane == active.id);
        let pid_cmd = project_id.to_string();
        let pane_cmd = active.id.clone();
        let cmd_btn = active.pty_id.map(|_| {
            // 命令库浮层开合动画的原点探针:报给本视图,按该叶子的 active pane 记(见
            // [`Self::note_cmd_anchor`])
            let anchor_pane = active.id.clone();
            let probe = anchor_probe(cx.entity(), move |area, bounds| {
                area.note_cmd_anchor(&anchor_pane, bounds)
            });
            ctrl_icon(
                gpui::SharedString::from(format!("term-cmds-{leaf_id}")),
                command_library::ICON_COMMANDS,
            )
            .relative()
            .when(cmd_open_here, |el| el.opacity(1.0).bg(ui::border_subtle()))
            .tip(t("commandLibrary", "buttonTooltip"))
            .on_click(cx.listener(move |this, _event, window, cx| {
                cx.stop_propagation();
                this.toggle_command_popover(&pid_cmd, &pane_cmd, window, cx);
            }))
            .child(probe)
        });
        // ⚑ N:图标是**文本字符**,不是 SVG(与 menu.rs 的 `✓ ` 同一套理由);
        // 宽度不固定,所以不复用上面那个 22×22 的方钮。
        let marker_pty = active.pty_id.filter(|_| marker_count > 0);
        let marker_pane_id = active.id.clone();
        let marker_btn = marker_pty.map(|pty_id| {
            // 标记浮层开合动画的原点探针:报给本视图,按该叶子的 active pane 记(见
            // [`Self::note_marker_anchor`])
            let anchor_pane = active.id.clone();
            let probe = anchor_probe(cx.entity(), move |area, bounds| {
                area.note_marker_anchor(&anchor_pane, bounds)
            });
            div()
                .id(gpui::SharedString::from(format!("markers-{leaf_id}")))
                .relative()
                .mr(px(MARKER_BTN_MARGIN_RIGHT))
                .px(px(6.0))
                .py(px(2.0))
                .rounded(px(3.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .cursor_pointer()
                .text_color(ui::text_muted())
                .hover(|el| el.text_color(ui::accent()).bg(ui::border_subtle()))
                // 两颗键帽取自键位表此刻生效的那份(用户改键 / mac 的 ⌘ 都跟着走)
                .tip(tr!(
                    "paneGroup",
                    "markerTooltip",
                    prev = crate::hotkeys::hotkey_tip("markerPrev"),
                    next = crate::hotkeys::hotkey_tip("markerNext")
                ))
                .on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.toggle_marker_popover(&marker_pane_id, pty_id, window, cx);
                }))
                .child("⚑")
                .child(div().child(marker_count.to_string()))
                .child(probe)
        });
        // SSH 交互(该组 active pane 的 AI SSH 调用记录)。没有记录时整个不画 —— 用
        // `children(Option)` 而不是画一个空占位,簇里不多出一格 gap、别的图标不挪位。
        // 按钮状态读自 pane 的 `SshActivityView`;它只在条数 / 在跑 / 失败 / 开合变了时
        // notify(经视图路径把这里标脏),输出分片不惊动这一层,见 `ssh_activity` 模块注释
        let ssh_btn = active.pty_id.and_then(|pty_id| {
            let view = store.terminal(pty_id)?.read(cx).ssh_activity().clone();
            let state = view.read(cx).button_state()?;
            Some(ssh_activity_button(leaf_id, view, state))
        });
        // 最大化 / 还原(v0.14.0)。只有真分了屏才画 —— 单格布局下「铺满」是空操作,
        // 常驻一颗按不动的按钮不如不画(原版 `layoutIsSplit || isMaximized` 同款)。
        let pid_max = project_id.to_string();
        let anchor_max = active.id.clone();
        let maximize_btn = (layout_is_split || is_maximized).then(|| {
            ctrl_icon(
                gpui::SharedString::from(format!("maximize-{leaf_id}")),
                if is_maximized {
                    ICON_RESTORE
                } else {
                    ICON_MAXIMIZE
                },
            )
            .tip(tr!(
                "paneGroup",
                if is_maximized {
                    "restorePane"
                } else {
                    "maximizePane"
                },
                shortcut = crate::hotkeys::hotkey_tip("toggleMaximize")
            ))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                cx.stop_propagation();
                let (pid, anchor) = (pid_max.clone(), anchor_max.clone());
                this.store.update(cx, |store, cx| {
                    store.toggle_maximized_leaf(&pid, &anchor, cx)
                });
            }))
        });
        let pid_right = project_id.to_string();
        let anchor_right = active.id.clone();
        let pid_down = project_id.to_string();
        let anchor_down = active.id.clone();
        let pid_close_leaf = project_id.to_string();
        let leaf_for_close = leaf_id.to_string();
        div()
            .ml_auto()
            // 页签区放不下时收窄的是它,不是这一簇(marker 浮层的锚点按簇宽算)
            .flex_none()
            .flex()
            .items_center()
            .gap(px(CTRL_GAP))
            .px(px(CTRL_CLUSTER_PAD))
            .children(marker_btn)
            .children(ssh_btn)
            .children(search_btn)
            .children(cmd_btn)
            .children(maximize_btn)
            .child(
                ctrl_icon(
                    gpui::SharedString::from(format!("split-right-{leaf_id}")),
                    ICON_SPLIT_RIGHT,
                )
                .tip(tr!(
                    "paneGroup",
                    "splitRightTooltip",
                    shortcut = crate::hotkeys::hotkey_tip("splitRight")
                ))
                .on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.store.update(cx, |store, cx| {
                        store.split_pane(
                            &pid_right,
                            &anchor_right,
                            SplitDirection::Horizontal,
                            window,
                            cx,
                        );
                    });
                })),
            )
            .child(
                ctrl_icon(
                    gpui::SharedString::from(format!("split-down-{leaf_id}")),
                    ICON_SPLIT_DOWN,
                )
                .tip(tr!(
                    "paneGroup",
                    "splitDownTooltip",
                    shortcut = crate::hotkeys::hotkey_tip("splitDown")
                ))
                .on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    this.store.update(cx, |store, cx| {
                        store.split_pane(
                            &pid_down,
                            &anchor_down,
                            SplitDirection::Vertical,
                            window,
                            cx,
                        );
                    });
                })),
            )
            .child(
                // × 仍是文本字形:hover 转 error 色要跟随文本色,VectorIcon
                // 进不去(见 ctrl_icon 的注释);盒子样式与图标钮同一套
                div()
                    .id(gpui::SharedString::from(format!("close-leaf-{leaf_id}")))
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(CTRL_BTN))
                    .h(px(CTRL_BTN))
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .opacity(0.6)
                    .text_color(ui::text_muted())
                    .hover(|el| {
                        el.opacity(1.0)
                            .bg(ui::border_subtle())
                            .text_color(ui::color_error())
                    })
                    .tip(tr!(
                        "paneGroup",
                        "closePaneTooltip",
                        shortcut = crate::hotkeys::hotkey_tip("closePane")
                    ))
                    // 控制条的 × 关的是**整组**,同样先确认(原版 closeLeaf)
                    .on_click(cx.listener(move |this, _event, window, cx| {
                        cx.stop_propagation();
                        let dead = pane_actions::leaf_is_dead(
                            this.store.read(cx),
                            &pid_close_leaf,
                            &leaf_for_close,
                        );
                        if this.swallows_close_click(dead, cx) {
                            return;
                        }
                        pane_actions::close_leaf(
                            this.store.clone(),
                            pid_close_leaf.clone(),
                            leaf_for_close.clone(),
                            window,
                            cx,
                        );
                    }))
                    .child("×"),
            )
    }
}
