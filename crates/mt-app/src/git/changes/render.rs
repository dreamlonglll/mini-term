use super::*;

// ─── 渲染 ─────────────────────────────────────────────────────

/// 文件行 / 目录行的行高(与分支弹窗、提交详情的文件列表同一口径)。
pub(super) const ROW_HEIGHT: f32 = git_ui::ROW_H;
/// 列表的上 / 下内边距。
pub(super) const LIST_PAD_TOP: f32 = 4.0;
pub(super) const LIST_PAD_BOTTOM: f32 = 8.0;
pub(super) const LIST_PAD_Y: f32 = LIST_PAD_TOP + LIST_PAD_BOTTOM;
/// 分组标题的高(`git_ui::SectionHeader` 的分组档)与上外边距。
pub(super) const GROUP_HEADER_H: f32 = 28.0;
pub(super) const GROUP_HEADER_GAP: f32 = 4.0;
/// ▾ 菜单的高度估计(三项、两三项带副标题、一条分隔线;按界面字号缩放)。只用来判断
/// 按钮下方放不放得下、放不下就往上弹,估大了只是离按钮远一点。
pub(super) const COMMIT_MENU_EST_H: f32 = 160.0;
/// 行右端「状态字母 / hover 时的操作按钮」那一格的宽:两颗图标按钮 + 间隙。
/// 每行都留同样宽,状态字母竖着对齐。
pub(super) const ROW_ACTIONS_W: f32 = git_ui::ICON_BUTTON * 2.0 + 2.0;
/// 文件行的组名(行内按钮 hover 才出现)。
pub(super) const ROW_GROUP: &str = "git-file-row";

impl Render for GitChanges {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_input(window, cx);
        self.sync_placeholder(window, cx);
        self.chevrons.drive(window);
        // 写操作闸每帧只问一次:行内 / 组操作按钮与提交按钮都按它画
        let gated = self.gated();
        let model = self.model(cx);
        let list = self.render_list(&model, self.ops_enabled_with(gated), cx);
        let commit_area = self.render_commit_area(&model, gated, cx);

        div()
            .size_full()
            .flex()
            .flex_col()
            // Ctrl/Cmd+Enter 提交、Ctrl/Cmd+Alt+Enter 提交并推送(键位绑在 hotkeys.rs,
            // 谓词 `"GitChanges > Input"` —— 与项目切换器的方向键同一套路)
            .key_context(CHANGES_CONTEXT)
            .on_action(cx.listener(Self::on_commit_action))
            .on_action(cx.listener(Self::on_commit_push_action))
            .when_some(self.metadata_warning.as_ref(), |el, warning| {
                el.child(git_ui::notice_line(
                    tr!("gitChanges", "metadataWarning", detail = warning),
                    ui::color_attention(),
                ))
            })
            .child(list)
            .child(commit_area)
    }
}

impl GitChanges {
    /// 列表区。有变更时是虚拟列表(只建视口里的行,见模块注释「列表只建看得见的行」);
    /// 加载中 / 取数失败 / 工作区干净三种整块占位仍是普通滚动容器。焦点、键位与滚动
    /// 位置两种形态共用。`enabled` = 整文件操作能不能点(同一帧只问一次闸)。
    pub(super) fn render_list(
        &mut self,
        model: &Rc<ListModel>,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let empty = self.changes.is_empty();
        let placeholder = if self.loading && empty && self.load_error.is_none() {
            Some(git_ui::loading_state(t("gitChanges", "loading")).into_any_element())
        } else if let Some(err) = self.load_error.as_deref().filter(|_| empty) {
            // 取变更失败顶替空态:否则读失败会被误认成「工作区是干净的」
            Some(
                git_ui::error_state(tr!("gitChanges", "loadFailed", detail = err))
                    .into_any_element(),
            )
        } else if empty {
            Some(self.render_clean_state(cx))
        } else {
            None
        };

        let list = div()
            .id("git-changes-list")
            .track_focus(&self.list_focus)
            .key_context(LIST_CONTEXT)
            .on_action(cx.listener(Self::on_select_prev))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_toggle_stage))
            .on_action(cx.listener(Self::on_open_diff))
            .on_action(cx.listener(Self::on_discard_key))
            .flex_1()
            .min_h(px(0.0));
        if let Some(placeholder) = placeholder {
            self.reveal_cursor = false;
            return list
                .overflow_y_scroll()
                .track_scroll(&self.list_scroll)
                .px(px(4.0))
                .pt(px(LIST_PAD_TOP))
                .pb(px(LIST_PAD_BOTTOM))
                .child(placeholder)
                .into_any_element();
        }

        // 游标行要滚进视野:行序按这一帧的模型找,偏移等 prepaint 量到这一帧的视口再算
        // (下面那块 canvas 排在虚拟列表**之前**,先于它 prepaint,改的偏移当帧就生效)
        let reveal = std::mem::take(&mut self.reveal_cursor)
            .then(|| self.cursor_row(model))
            .flatten()
            .and_then(|row| model.row_span(row));
        let scroll = self.list_scroll.clone();
        let rows = model.clone();
        let tree_mode = model.tree_mode;
        list.relative()
            .child(
                canvas(
                    move |bounds: Bounds<Pixels>, _window, _cx| {
                        let Some((top, bottom)) = reveal else {
                            return;
                        };
                        let mut offset = scroll.offset();
                        let viewport = f32::from(bounds.size.height);
                        offset.y = px(reveal_offset(f32::from(offset.y), top, bottom, viewport));
                        scroll.set_offset(offset);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full(),
            )
            .child(
                v_virtual_list(
                    cx.entity(),
                    "git-changes-rows",
                    model.sizes.clone(),
                    move |this, range, _window, cx| {
                        range
                            .filter_map(|ix| rows.rows.get(ix))
                            .map(|row| this.render_row(row, enabled, tree_mode, cx))
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.list_scroll)
                // 组件库给 base 挂的是双轴 `overflow_scroll`,关掉横轴,与换掉的
                // `overflow_y_scroll()` 一致(横向内容宽是拿首行量的,首帧量的是 min-content,
                // 会凭空多出一段横向可滚范围 —— 搜索弹窗同一处理)
                .overflow_x_hidden()
                .px(px(4.0))
                .pt(px(LIST_PAD_TOP))
                .pb(px(LIST_PAD_BOTTOM)),
            )
            .into_any_element()
    }

    /// 键盘游标所在的文件行在行序里是第几行(游标所在分组折起来了 / 目录收起了就没有)。
    pub(super) fn cursor_row(&self, model: &ListModel) -> Option<usize> {
        let (area, path) = self.cursor.as_ref()?;
        let index = model
            .members(*area)
            .iter()
            .copied()
            .find(|&i| self.changes[i].path == *path)?;
        model.file_row(*area, index)
    }

    /// 虚拟列表的一项。每种行画出来的高度与 [`ListRow::height`] 一致,宽度撑满(虚拟列表
    /// 按根布局排每一项,不像普通块容器那样自动撑满)。
    pub(super) fn render_row(
        &self,
        row: &ListRow,
        enabled: bool,
        tree_mode: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match row {
            ListRow::Header {
                area,
                count,
                collapsed,
            } => self.render_group_header(*area, *count, *collapsed, enabled, cx),
            ListRow::Dir {
                area,
                name,
                full_path,
                depth,
                collapsed,
                file_count,
            } => self.render_dir_row(
                *area,
                name.clone(),
                full_path.clone(),
                *file_count,
                *depth,
                *collapsed,
                cx,
            ),
            ListRow::File { area, index, depth } => {
                self.render_file_row(*index, *area, *depth, tree_mode, enabled, cx)
            }
        }
    }

    /// 工作区干净:32px 勾形图标 + 一句说明 + 「刷新」。
    ///
    /// 上下留白只用 `empty_state` 自带的那一份(24)。此前另叠了 `py(32)`,整块比列表区高,
    /// 「刷新」被挤到可见区外、只露一道顶边(E2E b/s75)。现在面板也会按内容给「更改」区
    /// 让高度(`git_panel::section_split`),这一块整个看得见。
    pub(super) fn render_clean_state(&self, cx: &mut Context<Self>) -> AnyElement {
        git_ui::empty_state(
            ICON_CLEAN,
            t("gitChanges", "empty"),
            Some(
                git_ui::action_button(
                    "git-changes-refresh",
                    t("gitChanges", "refresh"),
                    ButtonKind::Ghost,
                    true,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| this.load(cx)))
                .into_any_element(),
            ),
        )
        .into_any_element()
    }

    /// 分组标题:折叠箭头 + 标题 + 数量胶囊 + 右对齐的组操作(常显,保证可发现)。
    ///
    /// 上外边距 4 改成外面包一层的上内边距:虚拟列表把每一项当布局根排,根节点的外边距
    /// 靠不住;包一层不可交互的壳,点那 4px 照旧什么都不触发,看上去一模一样。
    pub(super) fn render_group_header(
        &self,
        area: Area,
        count: usize,
        collapsed: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (title, tone) = match area {
            Area::Conflicts => (t("gitCommit", "conflictsGroup"), ui::color_attention()),
            Area::Staged => (t("panels", "stagedChanges"), ui::text_muted()),
            Area::Unstaged => (t("panels", "unstagedChanges"), ui::text_muted()),
            Area::Untracked => (t("panels", "untrackedFiles"), ui::text_muted()),
        };
        let key = area.key();
        let turns = self.chevrons.value_open(
            &format!("group-{key}"),
            !collapsed,
            crate::fold_motion::OPEN_TURNS,
        );
        let mut header = SectionHeader::new(SharedString::from(format!("git-group-{key}")), title)
            .group()
            .collapsible(!collapsed)
            .chevron_turns(turns)
            .count(count);
        if area == Area::Conflicts {
            header = header.tone(tone).icon(ICON_ALERT);
        }
        // 组操作常显(保证可发现);共用图标按钮自己拦住冒泡,点它们不会顺带折叠
        match area {
            Area::Conflicts => {}
            Area::Staged => {
                header = header.trailing(
                    icon_button(
                        SharedString::from(format!("git-group-{key}-unstage")),
                        ICON_MINUS,
                        t("gitCommit", "unstageAll"),
                        enabled,
                    )
                    .when(enabled, |el| {
                        el.on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.unstage_all(window, cx);
                        }))
                    }),
                );
            }
            Area::Unstaged | Area::Untracked => {
                if area == Area::Unstaged {
                    header = header.trailing(
                        danger_icon_button(
                            SharedString::from(format!("git-group-{key}-discard")),
                            ICON_UNDO,
                            t("gitCommit", "discardAll"),
                            enabled,
                        )
                        .when(enabled, |el| {
                            el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                let paths = this.area_paths(area, cx);
                                this.discard(area, paths, window, cx);
                            }))
                        }),
                    );
                }
                header = header.trailing(
                    icon_button(
                        SharedString::from(format!("git-group-{key}-stage")),
                        ICON_PLUS,
                        t("gitCommit", "stageAll"),
                        enabled,
                    )
                    .when(enabled, |el| {
                        el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.stage_group(area, window, cx);
                        }))
                    }),
                );
            }
        }

        let toggle = cx.listener(move |this, _: &ClickEvent, _window, cx| {
            let open = this.collapsed_groups.remove(&area);
            if !open {
                this.collapsed_groups.insert(area);
            }
            this.chevrons.flip_open(
                format!("group-{}", area.key()),
                open,
                crate::fold_motion::OPEN_TURNS,
            );
            this.invalidate_model();
            cx.notify();
        });
        div()
            .w_full()
            .pt(px(GROUP_HEADER_GAP))
            .child(header.build().on_click(toggle))
            .into_any_element()
    }

    /// 目录行(共用的 [`git_ui::tree_dir_row`]:单链压缩后的名称 + 弱化文件数)。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn render_dir_row(
        &self,
        area: Area,
        name: String,
        full_path: String,
        file_count: usize,
        depth: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = dir_key(area, &full_path);
        let turns = self.chevrons.value_open(
            &format!("dir-{key}"),
            !collapsed,
            crate::fold_motion::OPEN_TURNS,
        );
        git_ui::tree_dir_row(
            SharedString::from(format!("git-dir-{key}")),
            name,
            file_count,
            depth,
            collapsed,
            turns,
        )
        .w_full()
        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
            let open = this.collapsed_dirs.remove(&key);
            if !open {
                this.collapsed_dirs.insert(key.clone());
            }
            this.chevrons
                .flip_open(format!("dir-{key}"), open, crate::fold_motion::OPEN_TURNS);
            this.invalidate_model();
            cx.notify();
        }))
        .into_any_element()
    }

    /// 文件行:文件图标 + 文件名(状态色,删除加删除线)+ 弱化目录 … 右端状态字母;
    /// hover 时字母让位给操作按钮。选中 = accent 淡底 + 左侧 2px accent 竖条
    /// (与 hover 底区分开)。
    pub(super) fn render_file_row(
        &self,
        index: usize,
        area: Area,
        depth: usize,
        tree_mode: bool,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(file) = self.changes.get(index) else {
            return div().into_any_element();
        };
        let status = status_in_area(file, area);
        let label = status_label_for(status);
        let color = status_color(status);
        let path = file.path.clone();
        let deleted = matches!(status, Some(GitStatus::Deleted));
        let selected = self.is_selected(area, &path);

        // tooltip:完整路径(重命名带旧路径)+ 状态说明(部分暂存另注一句)。文字等气泡
        // 真要弹出时才拼(上游等满停留时长才调构造闭包),这里只带拼它要的几样
        let tip_old = file
            .old_path
            .clone()
            .filter(|_| matches!(status, Some(GitStatus::Renamed)));
        let tip_path = path.clone();
        let tip_status = status.cloned();
        let tip_partial =
            file.staged_status.is_some() && file.unstaged_status.is_some() && !is_conflicted(file);

        let row_id = SharedString::from(format!("git-file-{}-{}", area.key(), path));
        let id_of =
            |what: &str| SharedString::from(format!("git-file-{what}-{}-{path}", area.key()));

        // 行内按钮(作用于选中集,见 `targets`)
        let mut actions = div()
            .absolute()
            .top_0()
            .right_0()
            .h_full()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(2.0))
            .invisible()
            .group_hover(ROW_GROUP, |s| s.visible());
        if matches!(area, Area::Unstaged | Area::Untracked) {
            actions = actions.child(
                danger_icon_button(
                    id_of("discard"),
                    ICON_UNDO,
                    if area == Area::Untracked {
                        t("gitCommit", "deleteUntracked")
                    } else {
                        t("gitChanges", "contextDiscard")
                    },
                    enabled,
                )
                .when(enabled, |el| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let paths = this.targets(area, index, cx);
                        this.discard(area, paths, window, cx);
                    }))
                }),
            );
        }
        match area {
            Area::Staged => {
                actions = actions.child(
                    icon_button(
                        id_of("unstage"),
                        ICON_MINUS,
                        t("panels", "unstage"),
                        enabled,
                    )
                    .when(enabled, |el| {
                        el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            let paths = this.targets(area, index, cx);
                            this.unstage(paths, None, window, cx);
                        }))
                    }),
                );
            }
            Area::Unstaged | Area::Untracked => {
                let stage = icon_button(id_of("stage"), ICON_PLUS, t("panels", "stage"), enabled);
                actions = actions.child(stage.when(enabled, |el| {
                    el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let paths = this.targets(area, index, cx);
                        this.stage(paths, None, window, cx);
                    }))
                }));
            }
            // 逐段解决(只管这一行,不跟选中集走):用不了时按钮压淡,tooltip 写明原因
            Area::Conflicts => {
                let available = self.segment_availability(&path);
                let usable = enabled && available.is_ok();
                let tip =
                    available.map_or_else(|reason| reason, |_| t("gitCommit", "segments.menu"));
                let seg_path = path.clone();
                actions = actions.child(
                    icon_button(id_of("segments"), ICON_MERGE, tip, usable).when(usable, |el| {
                        el.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.open_segments(seg_path.clone(), window, cx);
                        }))
                    }),
                );
            }
        }

        git_ui::list_row(row_id, selected)
            .group(ROW_GROUP)
            .pl(px(git_ui::tree_row_pad(depth)))
            .child(git_ui::file_label(&path, color, deleted, !tree_mode))
            .child(
                div()
                    .relative()
                    .flex_none()
                    .w(px(ROW_ACTIONS_W))
                    .h(px(git_ui::ICON_BUTTON))
                    .child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_end()
                            .pr(px(4.0))
                            .group_hover(ROW_GROUP, |s| s.invisible())
                            .child(git_ui::status_letter(label, color)),
                    )
                    .child(actions),
            )
            .tip_with(move |window, cx| {
                let tip = file_row_tip(
                    &tip_path,
                    tip_old.as_deref(),
                    tip_status.as_ref(),
                    tip_partial,
                );
                Tooltip::new(tip).build(window, cx)
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.click_row(area, index, event, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    // 右键没落在选中集上:先把选择换成这一行(与资源管理器同手感)
                    let path = this
                        .changes
                        .get(index)
                        .map(|c| c.path.clone())
                        .unwrap_or_default();
                    if !this.is_selected(area, &path) {
                        this.select_single(area, index);
                        cx.notify();
                    }
                    window.focus(&this.list_focus, cx);
                    let entries = this.file_menu(index, area, cx);
                    menu::show(event.position, entries, window, cx);
                }),
            )
            .into_any_element()
    }
}
