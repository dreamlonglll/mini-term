use super::*;

// ─── 渲染 ─────────────────────────────────────────────────────

/// 紧凑形态的 ↑↓ 计数(`↑2↓1`,见 [`SyncBadge::Compact`]):不带胶囊底,箭头与数字
/// 同色(待推送 success、待拉取 info,与两枚胶囊同一张语义表)。两项都是 0 时不画。
pub(super) fn compact_sync(ahead: usize, behind: usize) -> Option<gpui::Div> {
    if ahead == 0 && behind == 0 {
        return None;
    }
    let item = |shapes: &'static [mt_ui::icons::Shape], count: usize, color: gpui::Hsla| {
        div()
            .flex()
            .items_center()
            .child(git_ui::icon(shapes, git_ui::ICON_SIZE_XS, color))
            .child(
                git_ui::mono(div())
                    .text_color(color)
                    .child(count.to_string()),
            )
    };
    Some(
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(3.0))
            .px(px(4.0))
            .text_size(ui::font_px(11.0))
            .when(ahead > 0, |el| {
                el.child(item(git_ui::ICON_ARROW_UP, ahead, ui::color_success()))
            })
            .when(behind > 0, |el| {
                el.child(item(git_ui::ICON_ARROW_DOWN, behind, ui::color_info()))
            }),
    )
}

/// 计数收进 tooltip 之后([`SyncBadge::Hidden`])留的提示:分支图标右上角一个小圆点
/// (待拉取 info 优先、只有待推送 success),绝对定位、不占宽。`counts` 为 `None` 或
/// 两项都是 0 时原样返回图标。
pub(super) fn sync_dot_over(icon: impl IntoElement, counts: Option<(usize, usize)>) -> AnyElement {
    let color = match counts {
        Some((_, behind)) if behind > 0 => ui::color_info(),
        Some((ahead, _)) if ahead > 0 => ui::color_success(),
        _ => return icon.into_any_element(),
    };
    div()
        .relative()
        .flex_none()
        .child(icon)
        .child(
            div()
                .absolute()
                .top(px(-2.0))
                .right(px(-3.0))
                .size(px(6.0))
                .rounded_full()
                .bg(color),
        )
        .into_any_element()
}

/// 整面板的空态(没选项目 / 远程项目):与列表空态同一套「大图标 + 说明」。
pub(super) fn centered_hint(
    shapes: &'static [mt_ui::icons::Shape],
    text: &'static str,
) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(ui::bg_surface())
        .child(git_ui::empty_state(shapes, text, None))
        .into_any_element()
}
/// 这个仓库是不是项目根自己。
///
/// 两边的来源不同,只去尾部分隔符没法比:项目路径是用户填的(Windows 上是 `\`),
/// 而项目根仓库那条走 `Repository::workdir()`,libgit2 内部一律用 `/`
/// (`D:/Git/mini-term/`)。两边都转成 [`slash_form`](mt_core::path_key::slash_form)
/// (分隔符统一 `/`、去尾部分隔符,大小写原样)再比 —— 与 [`repo_detail`] 同一把尺。
///
/// (原版 `replace(/[\\/]+$/,'')` 只去尾巴,Windows 上项目根仓库永远判成「不是
/// 根」,「在终端中打开」会带上 cwd 覆盖并把页签改名成仓库名。)
pub(super) fn is_project_root_repo(repo_path: &str, project_path: &str) -> bool {
    use mt_core::path_key::slash_form;
    slash_form(repo_path) == slash_form(project_path)
}

/// 仓库在下拉/仓库栏里的**出处**:它的父目录相对项目根的那一段。
///
/// 仓库名取的是目录叶子名([`mt_project::git::discover_git_repos`]),于是
/// `services/api`、`legacy/api` 这类 monorepo 布局会在下拉里排出好几个一模
/// 一样的「api」,光看名字分不出是哪一个。叶子名撞车的前提就是父目录不同,
/// 所以补上这一段父级路径**一定**能区分。
///
/// 三档:
/// - 仓库就是项目根 / 项目的直接子目录 → `None`。深度 1 的名字在同一个父目录
///   下天然唯一,给每行都挂一条空信息的副标题只是噪音。
/// - 仓库在项目里更深处 → `services`、`packages/tools` 这样的相对段。
/// - 仓库在项目**外**(项目落在某个仓库的子目录里,`discover_repo_limited`
///   会向上找到它)→ 仓库自己的绝对路径(本机原生分隔符,与仓库栏 tooltip 同一写法,
///   见 [`native_path_display`]),免得只剩一个没有出处的叶子名。
pub(super) fn repo_detail(repo_path: &str, project_path: Option<&str>) -> Option<String> {
    use mt_core::path_key::slash_form;
    let repo = slash_form(repo_path);
    if repo.is_empty() {
        return None;
    }
    let project = project_path.map(slash_form).unwrap_or_default();
    // 项目根自己就是仓库:没有可补的出处
    if repo == project {
        return None;
    }
    let rel = if project.is_empty() {
        None
    } else {
        repo.strip_prefix(&format!("{project}/"))
    };
    match rel {
        // 项目内:掐掉叶子名,剩下的父级段就是出处(直接子目录 → 没有父级段 → None)
        Some(rel) => rel.rsplit_once('/').map(|(parent, _)| parent.to_string()),
        // 项目外(或压根没有项目路径):给绝对路径,总比只有一个叶子名强
        None => Some(native_path_display(&repo)),
    }
}

impl GitPanel {
    /// 仓库栏(`GitHistory.tsx:353-499`),重排成清晰的两段:左段仓库 + 分支,
    /// 右段图标按钮组。
    ///
    /// **按钮组永不让位、左段永不压到它上面**:左段 `flex_1 + min_w(0) + overflow_hidden`,
    /// 窄下去时按次序让位 ——
    ///
    /// 1. 仓库出处(父级路径)、仓库名先截(收缩权重远大于分支徽章;仓库名截到
    ///    [`REPO_NAME_FLOOR`] 为止);
    /// 2. 再截分支名(名字胶囊保底 40px,见 `git_ui::name_pill`);
    /// 3. 还放不下:↑↓ 计数从两枚胶囊收成一段紧凑字,再不行就不画(分支徽章的 tooltip
    ///    里逐行写着)。这一档靠 canvas 量左段宽度、按 [`bar_min_width`] 判,量值换档时
    ///    再画一帧;
    /// 4. 抽屉拖到最窄还放不下:仓库触发器收成只剩图标,再不行分支徽章也只剩图标
    ///    ([`BarSqueeze`]),两个入口都点得到。
    ///
    /// 两段的最小宽都**显式**给(名字截到保底之后的宽,见 [`repo_floor_width`] /
    /// [`branch_floor_width`]):自动最小宽按的是名字的完整宽度(截断只管溢出怎么画,
    /// 不改内容宽),仓库名会一个字都不让、全挤到分支名上;给 0 又会缩得比内容还窄。
    pub(super) fn render_repo_bar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let floor = self.bar_floor();
        self.sync_badge = sync_badge_for(self.bar_left_width, &floor);
        self.bar_squeeze = bar_squeeze_for(self.bar_left_width, &floor);
        let repo_icon_only = self.bar_squeeze != BarSqueeze::None;
        let info = self.selected_repo_info();
        let repo_name = info.map(|r| r.name.clone()).unwrap_or_default();
        let is_worktree = info.is_some_and(|r| r.is_worktree);
        // 选中的那个也只画叶子名,回头再看仍然认不出是哪个 `api` —— 名字前面
        // 补一段弱化的父级路径(与下拉里第二行同一个判据),完整路径挂 tooltip
        let detail =
            info.and_then(|r| repo_detail(&r.path.to_string_lossy(), self.project_path.as_deref()));
        let repo_path_tip: SharedString = native_path_display(&self.selected_repo).into();

        // ── 左段 ① 仓库 ──
        let repo_trigger = div()
            .id("git-repo-trigger")
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(26.0))
            .px(px(6.0))
            .rounded(px(4.0))
            .cursor_pointer()
            // 先于分支徽章收缩(权重 × 基准宽分摊,权重悬殊 = 几乎只缩它),缩到保底为止
            .map(|el| shrink(el, 1000.0))
            .min_w(px(if repo_icon_only {
                ICON_ONLY_WIDTH
            } else {
                repo_floor_width(&floor)
            }))
            .hover(|el| el.bg(git_ui::button_hover_bg()))
            .child(git_ui::icon(
                git_ui::ICON_REPO,
                git_ui::ICON_SIZE,
                ui::color_folder(),
            ))
            // 只剩图标时名字、出处、▾ 都不画(完整路径在 tooltip 里)
            .when(!repo_icon_only, |el| {
                el
                    // 父级路径先让位:栏子窄下去时先截它,仓库名与分支徽章保住
                    .when_some(detail, |el, detail| {
                        el.child(
                            div()
                                .min_w(px(0.0))
                                .max_w(px(90.0))
                                .map(|el| shrink(el, 1000.0))
                                .text_size(ui::font_px(12.0))
                                .text_color(ui::text_muted())
                                .truncate()
                                .child(format!("{detail}/")),
                        )
                    })
                    .child(
                        div()
                            .min_w(px(floor.repo_name))
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_primary())
                            .truncate()
                            .child(repo_name),
                    )
                    .when(is_worktree, |el| {
                        el.child(git_ui::icon(
                            git_ui::ICON_WORKTREE,
                            git_ui::ICON_SIZE_SM,
                            ui::text_muted(),
                        ))
                    })
                    .child(git_ui::icon(
                        git_ui::ICON_CHEVRON_DOWN,
                        git_ui::ICON_SIZE_SM,
                        ui::text_muted(),
                    ))
            })
            .tip(repo_path_tip)
            .on_click(cx.listener(|this, event: &ClickEvent, window, cx| {
                let entries = this.repo_menu(cx);
                menu::show(event.position(), entries, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let entries = this.repo_context_menu(cx);
                    menu::show(event.position, entries, window, cx);
                }),
            );

        // 量左段的宽:它是 flex_1 + basis 0,宽度只随栏宽与按钮组变,与里面画什么无关,
        // 按它换计数形态不会来回抖
        let this = cx.entity();
        let measure = canvas(
            move |bounds: Bounds<Pixels>, window, cx| {
                let stale = this.update(cx, |panel: &mut GitPanel, _cx| {
                    panel.bar_left_width = f32::from(bounds.size.width);
                    let floor = panel.bar_floor();
                    sync_badge_for(panel.bar_left_width, &floor) != panel.sync_badge
                        || bar_squeeze_for(panel.bar_left_width, &floor) != panel.bar_squeeze
                });
                if stale {
                    window.request_animation_frame();
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();
        let left = div()
            .relative()
            .flex()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .items_center()
            .gap(px(4.0))
            .child(repo_trigger)
            .children(self.render_branch_badge(cx))
            .child(measure);

        div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .h(px(BAR_HEIGHT))
            .flex_none()
            .pl(px(4.0))
            .pr(px(8.0))
            .border_b_1()
            .border_color(ui::border_subtle())
            .child(left)
            .child(self.render_bar_buttons(cx))
            .into_any_element()
    }

    /// 左段 ② 分支:分支图标 + 引用胶囊 + ↑n(待推送)/ ↓m(待拉取)计数胶囊。
    /// 点开分支弹窗。
    ///
    /// 胶囊配色与提交历史同一张语义表([`git_ui::RefKind`]):HEAD = accent;正在
    /// 查看另一个分支时按它是本地(success)还是远程(info)上色,前面的图标跟着换色
    /// 提示「这不是 HEAD」;detached HEAD 是语义表之外的状态,用警告色 + 提交图标。
    ///
    /// 宽度**随分支名自适应**:栏子挤不下时,仓库名先截到保底,再轮到分支名(胶囊自己
    /// 截断,保底 40px),最后计数收紧 / 收进 tooltip —— 次序见 [`Self::render_repo_bar`]。
    /// 截断时全名在 tooltip 里。
    pub(super) fn render_branch_badge(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let head = self.current_branch().map(str::to_string);
        let branch = self.view_branch.clone().or_else(|| head.clone())?;
        let viewing = self.view_branch.is_some() && self.view_branch != head;
        let detached = !viewing && head.as_deref().is_some_and(is_detached_label);
        let tracking = if viewing {
            None
        } else {
            self.meta.head_tracking.as_ref()
        };
        let viewing_remote = viewing
            && self
                .branches
                .iter()
                .any(|b| b.is_remote && b.name == branch);
        let (lead, lead_color, pill_color) = if detached {
            (git_ui::ICON_COMMIT, ui::text_muted(), ui::color_warning())
        } else if viewing {
            let kind = if viewing_remote {
                git_ui::RefKind::Remote
            } else {
                git_ui::RefKind::Local
            };
            let shapes = if viewing_remote {
                git_ui::ICON_REMOTE
            } else {
                git_ui::ICON_BRANCH
            };
            (shapes, kind.color(), kind.color())
        } else {
            (
                git_ui::ICON_BRANCH,
                ui::text_muted(),
                git_ui::RefKind::Head.color(),
            )
        };
        let tip: SharedString = branch_tip_lines(&branch, tracking, detached, viewing)
            .join("\n")
            .into();
        let (ahead, behind) = tracking.map_or((0, 0), |t| (t.ahead, t.behind));
        let busy = self.busy.is_some_and(GitBusy::touches_branch);
        let full = self.sync_badge == SyncBadge::Full;
        let floor = self.bar_floor();
        // 最窄一档只留图标(名字、计数都在 tooltip 里),转圈时圈顶替图标
        let icon_only = self.bar_squeeze == BarSqueeze::Icons;
        let min_width = if icon_only {
            ICON_ONLY_WIDTH
        } else {
            branch_floor_width(&floor) + counters_width(&floor, self.sync_badge)
        };
        let lead_icon = if icon_only && busy {
            ui::spinner(px(git_ui::ICON_SIZE), ui::accent()).into_any_element()
        } else {
            sync_dot_over(
                git_ui::icon(lead, git_ui::ICON_SIZE, lead_color),
                (self.sync_badge == SyncBadge::Hidden).then_some((ahead, behind)),
            )
        };

        Some(
            div()
                .id("git-branch-badge")
                .flex()
                .items_center()
                .gap(px(6.0))
                .h(px(26.0))
                .px(px(6.0))
                .rounded(px(4.0))
                // 仓库触发器缩到保底之后才轮到它;只缩分支名,计数按档整枚出现 / 收起
                .min_w(px(min_width))
                .cursor_pointer()
                .hover(|el| el.bg(git_ui::button_hover_bg()))
                .child(lead_icon)
                .when(!icon_only, |el| {
                    el.child(git_ui::name_pill(
                        "git-branch-badge-name",
                        branch,
                        pill_color,
                        None,
                    ))
                    .when(full && ahead > 0, |el| el.child(git_ui::ahead_pill(ahead)))
                    .when(full && behind > 0, |el| {
                        el.child(git_ui::behind_pill(behind))
                    })
                    .when(self.sync_badge == SyncBadge::Compact, |el| {
                        el.children(compact_sync(ahead, behind))
                    })
                    .when(busy, |el| {
                        el.child(ui::spinner(px(git_ui::ICON_SIZE_SM), ui::accent()))
                    })
                })
                .tip(tip)
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                    this.open_branch_popup(window, cx)
                }))
                .into_any_element(),
        )
    }

    /// 分支徽标的点击:打开分支弹窗。跑在面板自己的 update 里 —— 快照必须在这里取好
    /// 带过去,弹窗那边再 `panel.read` 会 panic(见 [`crate::git::branches::open`])。
    pub(super) fn open_branch_popup(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // 分支列表为空时懒加载一次(`GitHistory.tsx:422`)
        if self.branches.is_empty() {
            self.load_branches(cx);
        }
        let snapshot = self.branch_snapshot();
        crate::git::branches::open(cx.entity(), snapshot, window, cx);
    }

    /// 右段图标按钮组:Fetch / 拉取 / 推送 / 贮藏 / 刷新。
    pub(super) fn render_bar_buttons(&self, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.busy;
        // 有写操作在跑(面板自己的,或别的区占着写操作闸):其余写按钮一律禁用,提示口径
        // 见 [`write_tip`](见模块注释「写操作」)
        let gated = self.gated();
        let idle = busy.is_none() && !gated;
        let is = |op: GitBusy| busy == Some(op);
        let blocked = |op: GitBusy| (busy.is_some() && busy != Some(op)) || gated;
        let head = self.head_branch().map(str::to_string);
        let detached = self.current_branch().is_some_and(is_detached_label);
        let upstream = self.meta.head_tracking.as_ref().map(|t| t.upstream.clone());
        let mode = self.pull_mode_label();
        let tip = |op: Option<GitBusy>, normal: &dyn Fn() -> SharedString| -> SharedString {
            match write_tip(busy, gated, op) {
                WriteTip::Running => {
                    tr!("gitBranch", "busyTip", op = op.map_or("", GitBusy::label)).into()
                }
                WriteTip::Waiting => git_ui::gate_busy_text().into(),
                WriteTip::Normal => normal(),
            }
        };

        let fetch_tip = tip(Some(GitBusy::Fetch), &|| t("gitBranch", "fetchTip").into());
        let fetch = git_ui::busy_icon_button(
            "git-fetch",
            git_ui::ICON_FETCH,
            fetch_tip,
            is(GitBusy::Fetch),
            blocked(GitBusy::Fetch),
        )
        .when(idle, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.fetch(window, cx)))
        });

        let pull_tip = tip(Some(GitBusy::Pull), &|| {
            match &upstream {
                Some(upstream) => tr!(
                    "gitBranch",
                    "pullTip",
                    target = upstream.as_str(),
                    mode = mode.as_str()
                ),
                None => tr!("gitBranch", "pullTipNoUpstream", mode = mode.as_str()),
            }
            .into()
        });
        let pull = git_ui::busy_icon_button(
            "git-pull",
            git_ui::ICON_PULL,
            pull_tip,
            is(GitBusy::Pull),
            blocked(GitBusy::Pull) || detached,
        )
        .when(idle && !detached, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.pull(window, cx)))
        })
        // 右键选拉取方式(会话级,不落盘)
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|this, event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                let entries = this.pull_mode_menu(cx);
                menu::show(event.position, entries, window, cx);
            }),
        );

        let push_tip = tip(Some(GitBusy::Push), &|| {
            if detached {
                return t("gitBranch", "pushTipDetached").into();
            }
            // 写真实的推送目标(与推送对话框同一份计划):目标可以不是上游 —— 从
            // origin/main 拉出来的 feature 推的是 origin/feature
            let plan = match &self.meta.push_plan {
                Some(PushPlan::Ready(target)) => Some(target),
                _ => None,
            };
            match (plan, &upstream, &head) {
                (Some(target), _, _) if !target.sets_upstream() => {
                    tr!("gitBranch", "pushTip", target = target.label())
                }
                (Some(_), _, Some(head)) | (None, None, Some(head)) => {
                    tr!("gitBranch", "pushTipNoUpstream", branch = head.as_str())
                }
                // 计划没读到:退回按上游写
                (None, Some(upstream), _) => {
                    tr!("gitBranch", "pushTip", target = upstream.as_str())
                }
                _ => t("gitBranch", "pushTipDetached").to_string(),
            }
            .into()
        });
        let push = git_ui::busy_icon_button(
            "git-push",
            git_ui::ICON_PUSH,
            push_tip,
            is(GitBusy::Push),
            blocked(GitBusy::Push) || detached,
        )
        .when(idle && !detached, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.open_push_dialog(None, window, cx)
            }))
        });

        // 贮藏:开贮藏列表;有贮藏时旁边挂数量胶囊(数量随分支快照一起读),tooltip 带数
        let stash_count = self.meta.stash_count;
        let stash_tip = tip(None, &|| {
            if stash_count > 0 {
                tr!("gitStash", "countTip", count = stash_count).into()
            } else {
                t("gitStash", "title").into()
            }
        });
        let stash_button = git_ui::icon_button("git-stash", git_ui::ICON_STASH, stash_tip, idle)
            .when(idle, |el| {
                el.on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_stash_list(window, cx)
                    }),
                )
            });
        let stash = div()
            .flex()
            .flex_none()
            .items_center()
            .child(stash_button)
            .when(stash_count > 0, |el| {
                el.child(git_ui::count_pill(stash_count, ui::text_muted()))
            });

        // 刷新:后台重载期间转小圈(列表保留旧内容,不清空)
        let refreshing = self.branches_loading;
        let refresh = git_ui::busy_icon_button(
            "git-refresh",
            git_ui::ICON_REFRESH,
            t("gitBranch", "refreshTip"),
            refreshing,
            false,
        )
        .when(!refreshing, |el| {
            el.on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.refresh_all(cx)))
        });

        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(2.0))
            .child(fetch)
            .child(pull)
            .child(push)
            .child(stash)
            .child(refresh)
            .into_any_element()
    }

    /// 拉取方式菜单。当前选中的打勾;选了只改方式,不立刻拉。第一项「跟随 git 配置」
    /// (默认)= 不带方式参数拉,括号里写配置里的方式;下面三项是本次会话手选。
    pub(super) fn pull_mode_menu(&self, _cx: &mut Context<Self>) -> Vec<menu::MenuEntry> {
        let current = PULL_MODE.with(Cell::get);
        let mark = |on: bool| if on { "✓ " } else { "　" };
        let follow = tr!(
            "gitBranch",
            "modeFollowConfigItem",
            mode = config_pull_name(&self.meta.pull_config)
        );
        let mut entries = vec![
            menu::MenuEntry::Header(t("gitBranch", "pullModeHeader").into()),
            MenuItem::new(format!("{}{follow}", mark(current.is_none())))
                .on_click(|_window, _cx| PULL_MODE.with(|m| m.set(None)))
                .into(),
        ];
        for (mode, key) in [
            (PullMode::Merge, "modeMergeItem"),
            (PullMode::Rebase, "modeRebaseItem"),
            (PullMode::FfOnly, "modeFfOnlyItem"),
        ] {
            entries.push(
                MenuItem::new(format!(
                    "{}{}",
                    mark(current == Some(mode)),
                    t("gitBranch", key)
                ))
                .on_click(move |_window, _cx| PULL_MODE.with(|m| m.set(Some(mode))))
                .into(),
            );
        }
        entries
    }

    /// 进行中操作横幅:attention 语义 —— 左侧 3px 竖条 + 淡底 + 图标 + 加粗动词 +
    /// 冲突数胶囊,右对齐「继续」(主)「中止」(次)。仍有冲突时「继续」禁用。
    ///
    /// `op` 是要画的那一条:平时就是 `meta.op`,收起动画里是消失前的最后一条。
    pub(super) fn render_op_banner(
        &self,
        op: RepoOpState,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let kind = op.kind?;
        let conflicted = op.conflicted;
        let color = ui::color_attention();
        let verb = t(
            "gitBranch",
            match kind {
                RepoOpKind::Merge => "bannerMerge",
                RepoOpKind::Rebase => "bannerRebase",
                RepoOpKind::CherryPick => "bannerCherryPick",
                RepoOpKind::Revert => "bannerRevert",
            },
        );
        let busy = self.busy;
        let idle = busy.is_none() && !self.gated();
        let continuing = busy == Some(GitBusy::Continue);
        let aborting = busy == Some(GitBusy::Abort);
        let can_continue = conflicted == 0 && idle;

        // 横幅底已经染过一层 attention,冲突数胶囊的字色按染过的底算对比度
        let banner_bg = ui::with_alpha(color, 0.12);
        let status = if conflicted > 0 {
            git_ui::pill_on(
                tr!("gitBranch", "conflictCount", count = conflicted),
                color,
                git_ui::ground_with(banner_bg),
            )
            .into_any_element()
        } else {
            div()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(12.0))
                .text_color(ui::text_muted())
                .child(t("gitBranch", "conflictsResolved"))
                .into_any_element()
        };

        let continue_label = if continuing {
            tr!("gitBranch", "busyTip", op = GitBusy::Continue.label())
        } else {
            t("gitBranch", "continue").to_string()
        };
        let continue_button = git_ui::action_button(
            "git-op-continue",
            continue_label,
            git_ui::ButtonKind::Primary,
            can_continue,
        )
        .when(conflicted > 0, |el| {
            el.tip(tr!("gitBranch", "continueBlocked", count = conflicted))
        })
        .when(can_continue, |el| {
            el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.continue_op(kind, window, cx)
            }))
        });
        let abort_label = if aborting {
            tr!("gitBranch", "busyTip", op = GitBusy::Abort.label())
        } else {
            t("gitBranch", "abort").to_string()
        };
        let abort_button =
            git_ui::action_button("git-op-abort", abort_label, git_ui::ButtonKind::Ghost, idle)
                .when(idle, |el| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.confirm_abort(kind, window, cx)
                    }))
                });

        Some(
            div()
                .relative()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.0))
                .h(px(BAR_HEIGHT))
                .pl(px(12.0))
                .pr(px(8.0))
                .bg(banner_bg)
                .border_b_1()
                .border_color(ui::border_subtle())
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(3.0))
                        .bg(color),
                )
                .child(git_ui::icon(
                    if conflicted > 0 {
                        git_ui::ICON_ALERT
                    } else {
                        git_ui::ICON_MERGE
                    },
                    git_ui::ICON_SIZE,
                    color,
                ))
                .child(
                    div()
                        .flex_none()
                        .text_size(ui::font_px(13.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(ui::text_primary())
                        .child(verb),
                )
                .child(status)
                .child(div().flex_1())
                .child(continue_button)
                .child(abort_button)
                .into_any_element(),
        )
    }

    /// 「更改」标题栏右侧的视图切换(树/列表)。原先住在 GitChanges 的工具栏里,
    /// 工具栏撤掉后上移到这。
    pub(super) fn render_view_mode_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let tree_mode = self.store.read(cx).git_changes_view_mode() == "tree";
        // 显示的是「点了会切到」的那种视图(提交详情 / 提交 diff 左栏用同一颗)
        git_ui::view_mode_toggle("git-changes-view-mode", tree_mode)
            .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                // 按钮在 header 里:冒泡由共用图标按钮拦住,不会连带把整块折叠
                this.store.update(cx, |store, cx| {
                    let next = if store.git_changes_view_mode() == "tree" {
                        "list"
                    } else {
                        "tree"
                    };
                    store.set_git_changes_view_mode(next, cx);
                });
                // GitChanges 不 observe store,列表要重排得显式踢它一脚
                this.changes.update(cx, |_, cx| cx.notify());
                cx.notify();
            }))
            .into_any_element()
    }

    /// 仓库下拉。
    ///
    /// **走 [`menu`] 而不是自绘 absolute 面板**:点外关 / Esc / 贴边收拢 / 焦点
    /// 还原四件事那边已经做全了。代价是行内没法画「选中行 accent 底色」与右侧
    /// 分支胶囊的独立配色 —— 选中用 `✓ ` 前缀(菜单基件本来的勾选方案),
    /// 分支放右侧的弱化标签位。
    ///
    /// 重名仓库(monorepo 里的一堆 `api`)靠 [`repo_detail`] 补的第二行父级路径
    /// 区分,缩进与标题对齐 —— `✓ ` / 全角空格前缀本身就占一格。
    pub(super) fn repo_menu(&self, cx: &mut Context<Self>) -> Vec<menu::MenuEntry> {
        let this = cx.entity();
        let project = self.project_context.clone();
        self.repos
            .iter()
            .map(|repo| {
                let path = repo.path.to_string_lossy().to_string();
                let selected = path == self.selected_repo;
                let label = if selected {
                    format!("✓ {}", repo.name)
                } else {
                    format!("　{}", repo.name)
                };
                let mut item = MenuItem::new(label);
                if let Some(detail) = repo_detail(&path, self.project_path.as_deref()) {
                    item = item.detail(format!("　{detail}"));
                }
                if let Some(branch) = &repo.current_branch {
                    item = item.shortcut(if repo.is_worktree {
                        format!("⎇ {branch}")
                    } else {
                        branch.clone()
                    });
                }
                let this = this.clone();
                let project = project.clone();
                item.on_click(move |_window, cx| {
                    let path = path.clone();
                    this.update(cx, |this, cx| {
                        if this.project_context == project {
                            this.select_repo(path, cx);
                        }
                    });
                })
                .into()
            })
            .collect()
    }

    /// 仓库栏右键菜单(`GitHistory.tsx:300-329`)。
    pub(super) fn repo_context_menu(&self, cx: &mut Context<Self>) -> Vec<menu::MenuEntry> {
        let Some(repo) = self.selected_repo_info().cloned() else {
            return Vec::new();
        };
        let store = self.store.clone();
        let project_id = self.project_id.clone();
        let project_path = self.project_path.clone().unwrap_or_default();
        let repo_path = repo.path.to_string_lossy().to_string();
        // 项目根仓库不带 cwd 覆盖(默认就是项目根)
        let same_as_root = if self
            .project_context
            .as_ref()
            .is_some_and(GitContext::is_remote)
        {
            repo_path.trim_end_matches('/') == project_path.trim_end_matches('/')
        } else {
            is_project_root_repo(&repo_path, &project_path)
        };
        let title = if repo.is_worktree {
            format!(
                "⎇ {}",
                repo.current_branch.clone().unwrap_or(repo.name.clone())
            )
        } else {
            repo.name.clone()
        };
        let this = cx.entity();
        let Some(repo_for_worktree) = self.repo_context() else {
            return Vec::new();
        };
        let worktree_project_id = project_id.clone();
        let worktree_project = self.project_context.clone();

        vec![
            menu::item(
                t("gitHistoryContent", "openInTerminal"),
                move |window, cx| {
                    let Some(project_id) = project_id.clone() else {
                        return;
                    };
                    let (cwd, title) = if same_as_root {
                        (None, None)
                    } else {
                        (Some(repo_path.clone()), Some(title.clone()))
                    };
                    let opened = store.update(cx, |store, cx| {
                        let pane =
                            store.new_terminal_with_cwd(&project_id, None, None, cwd, window, cx);
                        if let (Some(pane), Some(title)) = (pane.as_ref(), title) {
                            store.rename_pane(&project_id, pane, &title, cx);
                        }
                        pane.is_some()
                    });
                    if opened {
                        crate::workbench_area::activate_terminal_page(window, cx);
                    }
                },
            ),
            menu::separator(),
            menu::item(
                t("gitHistoryContent", "manageWorktrees"),
                move |window, cx| {
                    let this = this.clone();
                    let project = worktree_project.clone();
                    crate::git::worktree::open(
                        // 单仓库模式:discover_repos = false
                        repo_for_worktree.clone(),
                        false,
                        worktree_project_id.clone(),
                        move |cx| {
                            this.update(cx, |this, cx| {
                                if this.project_context == project {
                                    this.load_repos(cx);
                                }
                            });
                        },
                        window,
                        cx,
                    );
                },
            ),
        ]
    }
}
