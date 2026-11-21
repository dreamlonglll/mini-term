use super::*;

impl Workspace {
    /// 弹窗 / 用量面板毛玻璃快照的生命周期对账:返回 `(frost_wanted, frost_alpha)`。
    fn sync_frost_snapshot(&mut self, window: &mut Window, cx: &mut Context<Self>) -> (bool, f32) {
        // 弹窗毛玻璃背板:Dialog 族或用量面板**从无到有的第一帧**抓一次快照
        // (那一刻 DWM 的上一帧还没有弹窗),期间沿用,全关即弃 —— 开着时再抓
        // 会把弹窗自己抓进去。抓帧同步(PrintWindow 有窗口线程亲和性,也快),
        // 模糊丢后台 —— 同步跑 debug 构建的模糊会把弹窗首帧拖出「慢半拍」
        // (用户实测);玻璃晚一两帧到,压暗层与弹窗本体零延迟。
        //
        // 图片放大浮层(`image_lightbox`)由文档页签自己 `deferred` 画出来,根层
        // 不持有它,只按 overlay 栈里有没有它来垫玻璃 —— 它打开的那次 notify 会把
        // 祖先视图一路标脏,这里必定在同一帧里重跑,抓到的仍是没有浮层的上一帧。
        //
        // 玻璃进场随遮罩淡入(`dialog_host::frost_alpha`),不「啪」地出现:快照到货那一刻
        // 起淡入。退场**当场撤**(用户定稿):快照可能是几分钟前抓的,随遮罩淡出就是旧画面
        // 叠在实时内容上的残影;遮罩压暗照常渐出。「有弹窗」只认开着的,退场中的不算。
        //
        // 图却留到外壳全部收完(`frost_keep`):退场途中又开弹窗就从零淡入、沿用这张,
        // **不重抓** —— 这时抓到的上一帧里有正在收起的那层(抓进去就是一团糊在新弹窗背后)。
        let dialog_open = dialog_host::has_open(window, cx);
        let lightbox_open = overlay::contains(overlay::key(overlay::kind::IMAGE_LIGHTBOX));
        let frost_target = dialog_open || self.usage_open || lightbox_open;
        let frost_alpha = dialog_host::frost_alpha(frost_target, self.frost.is_some(), window, cx);
        let frost_wanted = frost_alpha > 0.0;
        let frost_keep =
            frost_target || dialog_host::is_mounted(window, cx) || overlay_motion::lingering(cx);
        if frost_target {
            if self.frost.is_none()
                && self.frost_task.is_none()
                && let Some(raw) = frost::capture_raw(window)
            {
                self.frost_task = Some(cx.spawn(async move |this, cx| {
                    let img = cx
                        .background_executor()
                        .spawn(async move { frost::finish(raw) })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        this.frost_task = None;
                        if let Some(img) = img {
                            // 防御:按上面的门槛这里不该已有旧图,真有也得先把它
                            // 的纹理摘掉再换。不在任何窗口的更新里,`drop_image`
                            // 自己遍历得到本窗口;紧跟的 notify 让下一帧先重画再
                            // 呈现,旧场景不会拿着已摘的图块再呈现一遍
                            if let Some(old) = this.frost.replace(img) {
                                cx.drop_image(old, None);
                            }
                            cx.notify();
                        }
                    });
                }));
            }
        } else if !frost_keep && (self.frost.is_some() || self.frost_task.is_some()) {
            // 外壳全部收完了(或还没到货就全关了)才丢。
            // 快照是 `img(Arc<RenderImage>)` 直接画的,不经资源缓存,但上传进图集的
            // 纹理(约 1/4 窗口大小)只有 `drop_image` 摘得掉 —— 只清字段的话每开
            // 一次弹窗就漏一张。渲染途中必须把当前窗口递进去(它此刻被摘出了
            // `App.windows`,见 `file_viewer::mermaid::release_mermaid_assets`);本帧不再画它,
            // 上一帧的场景画完本帧就被替换,摘掉是安全的
            if let Some(old) = self.frost.take() {
                cx.drop_image(old, Some(window));
            }
            self.frost_task = None;
        }

        (frost_wanted, frost_alpha)
    }

    /// 中间栏(项目列表 + 文件树)及其开合滑入/滑出出场层。
    fn render_middle_column(
        &mut self,
        middle: &[f64],
        columns: &[f64],
        middle_visible: bool,
        window: &mut Window,
    ) -> (gpui::AnyElement, Option<gpui::Div>) {
        let store_for_middle = self.store.clone();
        // 中间栏是**上下**结构:ProjectList 在上、FileTree 在下(`App.tsx:501-512`
        // 的 `<Allotment vertical>`,minSize 100/120、无上限,高度落 middleColumnSizes)
        //
        // ⚠️ **只给上面那块播种,下面那块留空**(与三栏那组同一手法)。两块都传
        // `.size()` 的话,两个绝对值之和几乎不可能正好等于容器高,gpui-component
        // 会在首帧之后 `adjust_to_container_size()` 把它们**按比例**摊回容器高 ——
        // 存的 300/400 于是被拉成 583/777,「上次拖的高度」永远保不住(实测)。
        // 留空的那块 `initial_size` 是 `None`,自动吸收差额,上面那块就纹丝不动。
        let middle_group = self
            .middle_group(middle)
            .on_resize(move |state, _window, cx| {
                let sizes: Vec<f64> = state
                    .read(cx)
                    .sizes()
                    .iter()
                    .map(|p| f32::from(*p) as f64)
                    .collect();
                store_for_middle.update(cx, |store, cx| store.set_middle_column_sizes(sizes, cx));
            });
        // 中栏开合(见 [`MiddleMotion`]):开了 → 栏里内容从左滑入;收了 → 出场层画旧内容
        // 向左滑出。首帧只记下,不播
        let now = std::time::Instant::now();
        if let Some(was) = self.middle_seen
            && was != middle_visible
        {
            // 上一场还在飞就是半路掉头:从眼下的位置接着走(见 [`MiddleMotion`])
            let from = self
                .middle_motion
                .as_ref()
                .filter(|motion| motion.progress.running_at(now))
                .map_or(0.0, |motion| {
                    motion_kit::reversed_progress(motion.progress.value_at(now))
                });
            self.middle_motion = mt_ui::motion::animations_enabled().then(|| MiddleMotion {
                entering: middle_visible,
                progress: motion_kit::resume_band(from, mt_ui::motion::PANEL_SWAP, now),
            });
        }
        self.middle_seen = Some(middle_visible);
        let middle_frame = self.middle_motion.as_ref().and_then(|motion| {
            motion
                .progress
                .running_at(now)
                .then(|| (motion.entering, motion.progress.value_at(now)))
        });
        match middle_frame {
            Some(_) => window.request_animation_frame(),
            None => self.middle_motion = None,
        }
        let middle_content: gpui::AnyElement = match middle_frame {
            // 滑入:同尺寸的 absolute 层只动 left,栏本身裁掉露在左边外的部分
            Some((true, progress)) => div()
                .size_full()
                .relative()
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .h_full()
                        .w_full()
                        .left(gpui::relative(middle_content_left(true, progress)))
                        .child(middle_group),
                )
                .into_any_element(),
            _ => middle_group.into_any_element(),
        };
        // 滑出:栏已隐藏(上面那份进的是不可见的 panel,不会被画),出场层里画另一份。
        // 拦鼠标的是里层末尾那张罩(随内容滑、被外壳裁剪,只罩内容此刻还露着的那块):
        // 外壳上的 `occlude` 挡不住自己的子元素(滑走中的项目行照样能点、能起拖拽),
        // 整宽常驻又会在内容滑走后继续吞掉终端区左段的点击
        let middle_exit = match middle_frame {
            Some((false, progress)) => Some(
                div()
                    .absolute()
                    .top_0()
                    .left(px(activity_bar::WIDTH))
                    .h_full()
                    .w(px(columns[0] as f32))
                    .overflow_hidden()
                    .child(native_view::occluder())
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .h_full()
                            .w_full()
                            .left(gpui::relative(middle_content_left(false, progress)))
                            .child(self.middle_group(middle))
                            .child(div().absolute().inset_0().occlude()),
                    ),
            ),
            _ => None,
        };

        (middle_content, middle_exit)
    }

    /// 左侧窄边条(ActivityBar)。
    fn render_activity_bar(
        &self,
        middle_visible: bool,
        global_status: crate::tree::StatusLight,
        unread: usize,
        update_shown: Option<Arc<crate::update_check::ReleaseInfo>>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        // 左侧窄边条(ActivityBar):折叠中间栏 / AI 历史 / 用量统计 / 设置。
        //
        // 尺寸与配色照抄 `src/components/ActivityBar.tsx`(44px 宽、32px 方按钮、
        // 18px 图标、激活态左侧 accent 竖条);图形是原版那几条 SVG path 的
        // 逐点搬运,见 [`activity_bar`] 模块注释(以及「为什么不用 IconName」)。
        // 原版 8 颗按钮至此全部就位(BB-b 补上最后一颗 SSH);末尾那颗
        // 「跳到已完成」是 GPUI 独有的。
        div()
            .id("activity-bar")
            // 视觉层稍后排在 columns 之后画;body 里另留一块 44px 占位。
            // 这样按钮右侧标签能盖住相邻内容,又仍低于后面的 drawer/toast/modal。
            .absolute()
            .left_0()
            .top_0()
            .w(px(activity_bar::WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.0))
            .py(px(8.0))
            .bg(ui::bg_surface())
            .border_r_1()
            .border_color(ui::border_subtle())
            .on_hover(cx.listener(Self::on_activity_bar_hover))
            .child(
                activity_bar::strip_button(
                    "toggle-middle",
                    activity_bar::PANEL,
                    if middle_visible {
                        t("app", "activityBar.collapse")
                    } else {
                        t("app", "activityBar.expand")
                    },
                    middle_visible,
                    self.activity_bar_hover.is_visible("toggle-middle"),
                    Self::activity_bar_item_hover_listener("toggle-middle", cx),
                )
                // 全局 AI 状态徽标挂在这颗按钮上(中间栏承载项目列表)。
                // 口径与原版一致:只反映 AI 状态(含第五档「等你处理」),
                // **error 不往上冒** —— 某个 shell `exit 1` 不该让整条边栏亮红点、
                // 盖住真在跑的 AI。徽标本体(含 ai-working 档的闪烁)在
                // `activity_bar::status_badge`。
                .when(global_status != crate::tree::StatusLight::Idle, |el| {
                    el.child(activity_bar::status_badge(global_status))
                })
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.store
                        .update(cx, |store, cx| store.toggle_middle_column(cx));
                })),
            )
            // ⚠️ **除「折叠中间栏」外全条都不给激活态**(恒传 `active = false`):
            // 抽屉、竖条、用量页开着没开着本身就贴在屏幕上一眼可见,边条再点亮一次
            // 是重复反馈,几颗方块各亮各的反而把第一颗真正需要读的状态淹了。
            // 第一颗是唯一例外 —— 中间栏收起后没有任何其他线索能看出它的开合。
            .child(
                activity_bar::strip_button(
                    "toggle-sessions",
                    activity_bar::SESSIONS,
                    t("app", "activityBar.sessions"),
                    false,
                    self.activity_bar_hover.is_visible("toggle-sessions"),
                    Self::activity_bar_item_hover_listener("toggle-sessions", cx),
                )
                .on_click(cx.listener(|this, _event, _window, cx| {
                    this.toggle_drawer(DrawerPanel::Sessions, cx)
                })),
            )
            // Git 变更抽屉。位置照原版:紧跟 Sessions、排在分隔线之前
            // (`ActivityBar.tsx:143-150`)。
            .child(
                activity_bar::strip_button(
                    "toggle-git",
                    activity_bar::GIT,
                    t("app", "activityBar.git"),
                    false,
                    self.activity_bar_hover.is_visible("toggle-git"),
                    Self::activity_bar_item_hover_listener("toggle-git", cx),
                )
                .on_click(
                    cx.listener(|this, _event, _window, cx| {
                        this.toggle_drawer(DrawerPanel::Git, cx)
                    }),
                ),
            )
            // 终端列表竖条(GPUI 版新增,原版边条没有这颗)。开关的是终端区
            // 右缘的**停靠竖条**而不是右抽屉,同样不给激活态(理由见上)。
            .child(
                activity_bar::strip_button(
                    "toggle-terminals",
                    activity_bar::TERMINALS,
                    t("app", "activityBar.terminals"),
                    false,
                    self.activity_bar_hover.is_visible("toggle-terminals"),
                    Self::activity_bar_item_hover_listener("toggle-terminals", cx),
                )
                .on_click(cx.listener(|this, _event, window, cx| {
                    let terminal_was_active = this.terminal_page_active(cx);
                    this.workbench_area
                        .update(cx, |area, cx| area.activate_terminal(window, cx));
                    this.store.update(cx, |store, cx| {
                        if terminal_was_active || !store.terminals_panel_visible() {
                            store.toggle_terminals_panel(cx);
                        }
                    });
                })),
            )
            .child(activity_bar::divider())
            .child(
                activity_bar::strip_button(
                    "toggle-usage",
                    activity_bar::STATS,
                    t("app", "activityBar.stats"),
                    false,
                    self.activity_bar_hover.is_visible("toggle-usage"),
                    Self::activity_bar_item_hover_listener("toggle-usage", cx),
                )
                .on_click(cx.listener(|this, _event, window, cx| this.toggle_usage(window, cx))),
            )
            // ⚠️ 分隔线之后四颗按钮的**顺序照原版**(`ActivityBar.tsx:155-172`):
            // 用量 → 设置 → SSH → 移动端。U 批把「移动端」排在了「设置」之前
            // (那条注释把原版位置写反了),随本批补 SSH 时一并归位。
            .child(
                activity_bar::strip_button(
                    "open-settings",
                    activity_bar::SETTINGS,
                    t("app", "activityBar.settings"),
                    false,
                    self.activity_bar_hover.is_visible("open-settings"),
                    Self::activity_bar_item_hover_listener("open-settings", cx),
                )
                .on_click(cx.listener(|this, _event, window, cx| {
                    settings::open_settings(this.store.clone(), None, window, cx);
                })),
            )
            // 「SSH 连接」面板(连接与分组的增删改)
            .child(
                activity_bar::strip_button(
                    "open-ssh",
                    activity_bar::SSH,
                    t("app", "activityBar.ssh"),
                    false,
                    self.activity_bar_hover.is_visible("open-ssh"),
                    Self::activity_bar_item_hover_listener("open-ssh", cx),
                )
                .on_click(cx.listener(|_this, _event, window, cx| {
                    ssh_panel::open(window, cx);
                })),
            )
            .child(
                activity_bar::strip_button(
                    "open-mobile-relay",
                    activity_bar::MOBILE,
                    t("app", "activityBar.mobile"),
                    false,
                    self.activity_bar_hover.is_visible("open-mobile-relay"),
                    Self::activity_bar_item_hover_listener("open-mobile-relay", cx),
                )
                .on_click(cx.listener(|_this, _event, window, cx| {
                    mobile_panel::open(window, cx);
                })),
            )
            // 「有新版本」按钮。**只在查到更新时才出现**(原版 `updateVersion &&`,
            // `ActivityBar.tsx:173-182`)。原版点一下直接外链到发布页;这里改成先开
            // 「发现新版本」弹窗看发布说明,下载 / 忽略此版本 / 稍后在弹窗里选。
            // 位置照原版:排在全部常规按钮之后(下面那颗「跳到已完成」是 GPUI
            // 独有的,继续留在最末)。
            .children(update_shown.map(|release| {
                activity_bar::update_button(
                    "open-update",
                    tr!("app", "update.title", version = release.version.as_str()),
                    self.activity_bar_hover.is_visible("open-update"),
                    Self::activity_bar_item_hover_listener("open-update", cx),
                )
                .on_click(move |_event, window, cx: &mut gpui::App| {
                    update_dialog::open(&release, window, cx);
                })
            }))
            // 未读完成计数:点一下跳到最先完成的那个 pane(旧版托盘绿灯的入口;
            // 原版边栏没有这颗按钮,所以借状态灯的「实心圆 + 勾」当图形)
            .when(unread > 0, |el| {
                el.child(
                    activity_bar::done_button(
                        "jump-attention",
                        t("app", "titleBar.status.done"),
                        self.activity_bar_hover.is_visible("jump-attention"),
                        Self::activity_bar_item_hover_listener("jump-attention", cx),
                    )
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.on_jump_attention(&JumpAttention, window, cx);
                    })),
                )
            })
    }

    /// 右侧悬浮抽屉(Sessions ⇄ Git)及其左侧的提交详情栏。
    fn render_right_drawer(
        &mut self,
        drawer_panel: Option<DrawerPanel>,
        drawer_width: f64,
        detail_pane: Option<Entity<git_history::CommitDetailPane>>,
        detail_width: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        // 右侧悬浮抽屉(Sessions ⇄ Git):**absolute 悬浮层**,贴右边缘盖在终端之上。
        //
        // 原版就是这个形态(`RightDrawer.tsx:67`:`absolute top-0 right-0 h-full
        // z-[45]`),GPUI 侧此前借 resizable 的第三栏实现,代价是**开合会改终端
        // 宽度、连带触发一次 PTY resize**(刷屏进程正在跑时肉眼可见地重排)。
        // 改成悬浮层之后终端尺寸不动,PTY 也就不再收到 SIGWINCH。
        //
        // **停在文件 / diff 页签上时改为挤占**:抽屉本身仍是同一个悬浮层(位置、动画、
        // 拖宽一概不变),只是页签区那一栏里垫了一块等宽的占位(上面的 `squeeze`),
        // 页签区变窄、抽屉落在让出来的那一截里,不再盖住 diff。终端页照旧悬浮 ——
        // 终端区与页签页不同时渲染,切页时终端拿到的始终是整栏宽,开合抽屉不会让它重排。
        // 进场时占位一步到位、抽屉照旧从右缘滑进那一截;收起那一刻占位就撤掉,抽屉在
        // 退场驻留的 400ms 里从页签区上方滑走;拖抽屉 / 详情栏左缘时占位逐帧跟着变。
        //
        // 层级对照:原版 `z-45` 压过 allotment 分隔条(35)、低于弹窗(50)——
        // 这里 `.children(...)` 的顺序等价:抽屉排在三栏之后、弹窗/菜单层之前。
        // 抽屉**不进 [`overlay`] 栈**(原版 `RightDrawer` 同样没压栈),
        // 所以它开着时全局快捷键照常生效。
        //
        // 动画三条(`styles.css:284-313`):
        // - 进场 `drawerSlideIn` 240ms:整层从 `translateX(100%)` 滑进来 ——
        //   gpui 没有 transform,改成把 `right` 从 `-width` 补到 0(等效);
        // - 退场 `drawerSlideOut` 140ms:反过来,期间**面板实体仍留在树上**
        //   (`drawer_exit` 驻留 400ms),否则内容会先空掉;
        // - 换面板 200ms:左右整幅 push(`motion_kit::push_pair`),方向跟段控件次序
        //   (会话在左、Git 在右);会话 / Git 是两个不同实体,同帧挂两份没问题。**只在
        //   换面板时播**(`drawer_swap` 由 `set_drawer` 驱动),开抽屉不播 —— 此前内容层的
        //   id 带面板名,每次开抽屉都是新 id,连同整层滑入一起再淡一遍;原版的
        //   `panelSwapIn`(opacity 0→1 + 右移 10px)按口径改成不淡入的方向性位移。
        //
        // ⚠️ 这三条在原版被**显式豁免** `prefers-reduced-motion`
        // (`styles.css:424-451`),所以 GPUI 侧不加任何减弱动效判定,始终播放。
        let drawer_swap = self.drawer_swap.frame(window);
        let drawer_layer = drawer_panel.map(|panel| {
            let leaving = self.right_drawer.is_none();
            let width = drawer_width as f32;
            // ⚠️ 抽屉里这两块**不套** [`cached_panel`](Git 面板里的「更改」「提交历史」
            // 两个子视图也没单独套):
            // ① 它们身上压着整层滑入滑出的 `with_animation`(此前还有换面板的
            //    `panelSwapIn` 改 opacity),而 opacity 是 paint 期烙进图元的、
            //    `reuse_paint` 原样重放 —— 命中缓存就会把某一帧的透明度定死。连带的
            //    `ml(10·(1−δ))` 在末几帧被布局按设备像素取整成 0,缓存键与终态相同,
            //    挡不住复用;动画的起止对面板又不可见,「动画期间不走缓存」没有干净的判据;
            // ② 收起时这两个实体压根不进元素树。开着时每帧确实整个重跑,但最重的
            //    「更改」列表已虚拟化(只建视口里那几行,见 `git_changes` 模块注释),
            //    剩下的开销与改动数无关;提交历史区每行按墙钟现算「x 分钟前」、没有
            //    定时 notify,套缓存会把它冻住。
            let panel_view = |panel: DrawerPanel| -> gpui::AnyElement {
                match panel {
                    DrawerPanel::Sessions => self.session_panel.clone().into_any_element(),
                    DrawerPanel::Git => self.git_panel.clone().into_any_element(),
                }
            };
            // 换面板:新旧两个面板实体整幅 push;收起途中不推(整层在滑走)
            let content = match drawer_swap {
                Some((old, progress, dir)) if !leaving && old != panel => motion_kit::push_pair(
                    panel_view(panel),
                    Some(panel_view(old)),
                    progress,
                    motion_kit::PushAxis::Horizontal,
                    dir,
                )
                .into_any_element(),
                _ => panel_view(panel),
            };
            // Git 面板选中提交时,抽屉左侧再挂一栏「提交详情」
            let detail_layer = detail_pane
                .clone()
                .zip(detail_width)
                .map(|(pane, detail)| {
                    self.render_commit_detail_layer(pane, width, detail, cx)
                        .into_any_element()
                })
                .or_else(|| {
                    self.detail_exit.as_ref().map(|exit| {
                        Self::render_detail_exit_layer(exit, width, window).into_any_element()
                    })
                });
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(width))
                .occlude()
                .flex()
                .flex_col()
                .bg(ui::bg_overlay())
                // 盖在文档页上时,这块从 HTML 预览 WebView 的显示与点击里让出来
                // (抽屉不进 overlay 栈,见 `native_view` 模块注释「输入的空域」)
                .child(native_view::occluder())
                .border_l_1()
                .border_color(ui::border_default())
                // `--shadow-overlay`(`RightDrawer.tsx:67`);gpui 侧用同一档
                // 阴影,与 `menu.rs` 的浮层一致
                .shadow_lg()
                .child(self.render_drawer_header(panel, window, cx))
                .child(
                    div()
                        .id("drawer-body")
                        .flex_1()
                        .min_h(px(0.0))
                        .overflow_hidden()
                        .child(content),
                )
                // 左缘拖拽手柄:抽屉贴右边缘,把左缘往左拖 = 变宽,
                // 所以位移取 `start_x - 当前 x`(照抄 `RightDrawer.tsx:48`)
                .child(
                    div()
                        .id("drawer-resize-handle")
                        .absolute()
                        // 原版用 `-translate-x-1/2` 骑在边缘上;gpui 侧整条留在
                        // 抽屉内(压着左边框那 6px),免得被父级裁掉
                        .left_0()
                        .top_0()
                        .h_full()
                        .w(px(6.0))
                        .cursor_col_resize()
                        .hover(|el| el.bg(ui::with_alpha(ui::accent(), 0.4)))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(
                                move |this: &mut Self,
                                      event: &gpui::MouseDownEvent,
                                      _window,
                                      cx| {
                                    cx.stop_propagation();
                                    this.drawer_drag = Some(DrawerDrag {
                                        start_x: event.position.x,
                                        start_width: drawer_width,
                                        width: drawer_width,
                                    });
                                },
                            ),
                        ),
                )
                .children(detail_layer)
                .with_animation(
                    SharedString::from(if leaving {
                        "drawer-slide-out"
                    } else {
                        "drawer-slide-in"
                    }),
                    if leaving {
                        mt_ui::motion::OVERLAY_OUT.animation()
                    } else {
                        mt_ui::motion::OVERLAY_IN.animation()
                    },
                    move |el, delta| {
                        let offset = if leaving { delta } else { 1.0 - delta };
                        el.right(px(-width * offset))
                    },
                )
        });

        drawer_layer.map(IntoElement::into_any_element)
    }

    /// 用量统计全屏 Modal 浮层。
    fn render_usage_layer(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        // 原版 `w-[80vw] max-h-[85vh]` + 外壳默认 `pt-[10vh]`(`Modal.tsx:162`):
        // 左右各留 10vw,顶 10vh、底 5vh(10vh + 85vh = 95vh)
        let usage_viewport = window.viewport_size();
        // 进出场与 Dialog 族同一套外壳(`dialog_host::shell_frame`):遮罩压暗渐变,面板按最终
        // 尺寸排好、从顶边水平中点裁剪窗展开;收起反向播完才卸。退场那几帧遮罩不再拦点击、
        // 面板不再接鼠标(`toggle_usage` 那一下面板已经被告知不可见,定时器照停)
        let usage_frame = self
            .usage_panel
            .as_ref()
            .and_then(|_| dialog_host::shell_frame("usage-stats", self.usage_open, window, cx));
        let usage_layer = self
            .usage_panel
            .clone()
            .zip(usage_frame)
            .map(|(panel, frame)| {
                let open = frame.open;
                // 原版用量统计是 Modal(`UsageStatsModal.tsx:397`,fixed inset-0,
                // **盖住标题栏**),遮罩统一 `bg-black/50 backdrop-blur-sm`
                // (`Modal.tsx:171`)。毛玻璃由**根层那张共用快照**承担(与 Dialog
                // 族同一层,见 render 尾部)—— 挂根层而不是 body:body 版本盖不到
                // 标题栏、内嵌玻璃还会被拉伸错位,与设置弹窗观感不一致(用户实测)。
                // 遮罩上**按下**即关(mousedown 语义 —— 面板里按下、拖出去松手
                // 不误关);面板自己 stop_propagation 挡掉冒泡(`Modal.tsx:180` 同款)。
                div()
                    .absolute()
                    .inset_0()
                    .when(open, |el| el.occlude())
                    .bg(gpui::hsla(0.0, 0.0, 0.0, 0.5 * frame.progress))
                    // 自绘模态不进 overlay 栈:开着时 HTML 预览的 WebView 不收鼠标。退场那几帧
                    // 遮罩已经不拦点击,WebView 跟着恢复(与 Dialog 族同口径,画面本来就压在
                    // WebView 之上);正在收的面板那块由 `occlude_native` 让出来
                    .when(open, |el| {
                        el.child(native_view::input_blocker()).on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, _event, window, cx| {
                                if this.usage_open {
                                    this.toggle_usage(window, cx);
                                }
                            }),
                        )
                    })
                    .child(
                        overlay_motion::reveal_box(
                            div()
                                .absolute()
                                .top(usage_viewport.height * 0.10)
                                .left(usage_viewport.width * 0.10)
                                .right(usage_viewport.width * 0.10)
                                .bottom(usage_viewport.height * 0.05)
                                .when(open, |el| {
                                    el.occlude()
                                        // 面板内按下不冒泡到遮罩(原版 `Modal.tsx:180` 的
                                        // stopPropagation 同款),否则点面板空白处也会关窗
                                        .on_mouse_down(
                                            gpui::MouseButton::Left,
                                            |_event, _window, cx| cx.stop_propagation(),
                                        )
                                })
                                // 底色与边框色由外壳框体按插值矩形画(见下面 `chrome`),
                                // 这里只留版式:边框宽度照占位
                                .border_1()
                                .overflow_hidden()
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .justify_between()
                                        .px(px(12.0))
                                        .py(px(6.0))
                                        .bg(ui::bg_elevated())
                                        .child(
                                            div()
                                                .text_size(crate::ui::font_px(12.0))
                                                .text_color(ui::text_primary())
                                                .child(t("usageStats", "title")),
                                        )
                                        .child(
                                            div()
                                                .id("usage-close")
                                                .px(px(6.0))
                                                .text_color(ui::text_muted())
                                                .cursor_pointer()
                                                .hover(|el| el.text_color(ui::color_error()))
                                                // 走 toggle 而不是直接改标志位 —— 可见性要透给
                                                // 面板,否则关掉之后自动刷新定时器还在每 5s 跑
                                                .on_click(cx.listener(
                                                    |this, _event, window, cx| {
                                                        if this.usage_open {
                                                            this.toggle_usage(window, cx);
                                                        }
                                                    },
                                                ))
                                                .child("×"),
                                        ),
                                )
                                .child(div().flex_1().overflow_hidden().child(panel)),
                            frame.progress,
                        )
                        // 面板体自己的底色。**别省这一行** —— 少了它面板就是全透明的,
                        // 背后只剩那层 black/50,终端文字照样一个个看得清。
                        //
                        // Windows 上看不出来:根层那张毛玻璃快照垫在面板之下,透出来的是
                        // 模糊+压暗的画面,勉强能读。但快照走 `PrintWindow` 是 Windows 专有
                        // (见 `frost` 模块),**非 Windows 上 `frost` 恒为 `None`**,退化成
                        // 纯 black/50 —— 隔着一层黑纱看锐利的终端正文,读不了。
                        //
                        // 用 `bg_overlay` 而不是 `bg_surface` / `bg_elevated`:后两者在外置
                        // 主题包下会乘 `surface_opacity`(有背景图时要透出图),而浮层叠在任意
                        // 内容之上,半透明是拿可读性换观感 —— 判据见 `ui::Palette::from_pack`
                        // 里那一行注释。设置弹窗与 pane 预览 / 分支家族 / 日期选择器四处浮层
                        // 用的都是它,这里是唯一漏掉的一个。
                        .chrome(overlay_motion::chrome_style(|el| {
                            el.bg(ui::bg_overlay())
                                .rounded(px(6.0))
                                .border_1()
                                .border_color(ui::border_default())
                        }))
                        .interactive(open)
                        .occlude_native(!open),
                    )
            })
            // 上面压着弹窗时面板里的字不参与选区(见 `dialog_host::confine_background`)
            .map(|layer| dialog_host::confine_background(layer, window, cx));

        usage_layer.map(IntoElement::into_any_element)
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 窗口尺寸变了就让两组分栏重新以持久化的绝对像素做种(见该函数注释)——
        // 否则 gpui-component 会把还原进去的宽度按容器比例重算掉。
        self.reseed_resizables_on_viewport_change(window, cx);

        let (
            columns,
            middle,
            middle_visible,
            terminals_visible,
            drawer_width,
            unread,
            global_status,
            background,
        ) = {
            let store = self.store.read(cx);
            let config = store.config();
            let columns = config
                .layout_sizes
                .clone()
                .filter(|s| s.len() == 2)
                .unwrap_or_else(|| DEFAULT_COLUMNS.to_vec());
            let middle = config
                .middle_column_sizes
                .clone()
                .filter(|s| s.len() == 2)
                .unwrap_or_else(|| DEFAULT_MIDDLE.to_vec());
            (
                columns,
                middle,
                config.middle_column_visible,
                store.terminals_panel_visible(),
                store.right_drawer_width(),
                store.unread_done_count(),
                store.global_ai_light(),
                store.background_art().cloned(),
            )
        };
        let terminal_page_active = self.workbench_area.read(cx).is_terminal_active(cx);
        // 边条更新按钮:查到的新版本还要过「忽略此版本」那道闸
        let update_shown = {
            let ignored = self
                .store
                .read(cx)
                .config()
                .ignored_update_version
                .as_deref();
            self.update_release.clone().filter(|release| {
                update_check::should_announce(release, env!("CARGO_PKG_VERSION"), ignored)
            })
        };

        // 条件按钮可能在鼠标仍停在原坐标时从元素树消失,这时 GPUI 不保证再补一发
        // on_hover(false)。主动按当前可见性对账,避免它的旧计时把会话偷偷热身。
        let conditional_hover_gone = (unread == 0
            && self.activity_bar_hover.leave("jump-attention"))
            || (update_shown.is_none() && self.activity_bar_hover.leave("open-update"));
        if conditional_hover_gone {
            self.activity_bar_hover_task = None;
        }

        let (frost_wanted, frost_alpha) = self.sync_frost_snapshot(window, cx);

        let store_for_columns = self.store.clone();
        // 拖拽期间宽度自持,松手才落盘(与原版 `RightDrawer` 的 `onResizeEnd` 同)
        let drawer_width = self.drawer_drag.map(|d| d.width).unwrap_or(drawer_width);
        // 抽屉里正画着哪一块(退场动画驻留期间是刚收起的那块),以及 Git 抽屉左侧的提交详情栏
        // 有多宽 —— 画抽屉层与挤占页签区共用这一份
        let exiting = self.drawer_exit.as_ref().map(|e| e.panel);
        let drawer_panel = self.right_drawer.or(exiting);
        let detail_pane = match drawer_panel {
            Some(DrawerPanel::Git) => self.git_panel.read(cx).detail_pane(cx),
            _ => None,
        };
        let detail_width = detail_pane
            .as_ref()
            .map(|_| self.detail_width(drawer_width as f32, window, cx));
        // 提交详情栏刚没了(取消选中 / 关栏 / 换到会话面板)且抽屉还开着:起退场,播完再卸。
        // 整个抽屉收起时详情栏随抽屉一起滑走,不另播
        match detail_pane.clone().zip(detail_width) {
            Some(shown) => {
                self.detail_exit = None;
                self.detail_seen = Some(shown);
            }
            None => {
                if let Some((pane, width)) = self.detail_seen.take()
                    && self.right_drawer.is_some()
                    && mt_ui::motion::animations_enabled()
                {
                    self.detail_exit = Some(DetailExit {
                        pane,
                        width,
                        transition: mt_ui::motion::Transition::new(mt_ui::motion::OVERLAY_OUT),
                    });
                }
                if self
                    .detail_exit
                    .as_ref()
                    .is_some_and(|exit| !exit.transition.running() || self.right_drawer.is_none())
                {
                    self.detail_exit = None;
                }
            }
        }
        // 停在文件 / diff 页签上时,开着的抽屉(连同详情栏)挤窄页签区而不是盖在上面;
        // 终端页照旧悬浮(见 [`drawer_squeeze`])。只看 `right_drawer`:收起那一刻就把
        // 页签区放回原宽,退场动画里的抽屉照旧从页签区上方滑走。页签区所在那一栏的宽按
        // 视口减边条与中栏估(中栏宽是落盘的那份,拖分隔条时差几像素无妨 —— 只拿来兜
        // 「至少留多宽」那条底)
        let doc_panel_width = f32::from(window.viewport_size().width)
            - activity_bar::WIDTH
            - if middle_visible {
                columns[0] as f32
            } else {
                0.0
            };
        let squeeze = drawer_squeeze(
            terminal_page_active,
            self.right_drawer.is_some(),
            drawer_width as f32 + detail_width.unwrap_or(0.0),
            doc_panel_width,
        );

        let (middle_content, middle_exit) =
            self.render_middle_column(&middle, &columns, middle_visible, window);
        let columns_group = h_resizable("columns")
            .with_state(&self.columns_state)
            .child(
                resizable_panel()
                    .visible(middle_visible)
                    .size(px(columns[0] as f32))
                    .size_range(px(180.0)..px(700.0))
                    .child(middle_content),
            )
            // 面板切换竖条停靠在终端区**里侧**右缘(固定 44px、不进 resizable
            // 的分栏账),与 VS Code 终端面板右侧的列表同位。开合会改终端宽度
            // 并触发一次 PTY resize —— 停靠面板的正常代价,与折叠中间栏同档;
            // 悬浮的右抽屉(Sessions/Git)另有考量,见下方 drawer_layer 注释。
            //
            // 停在文件 / diff 页签上时,抽屉挤占的那一截是这一栏**里面**最右的一块占位
            // (`squeeze`),而不是缩这一栏本身:三栏的 `ResizableState` 一见容器变宽变窄
            // 就按比例重摊各栏(见 `reseed_resizables_on_viewport_change`),中栏会跟着
            // 抽屉开合伸缩。终端页时竖条在、占位不在,文件页时反过来,两者不同时出现。
            .child(
                resizable_panel().child(
                    div()
                        .size_full()
                        .flex()
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                // ⚠️ 终端区**不套** [`cached_panel`]:它就是每一拍
                                // 真在变的那块内容,套上等于每帧必然未命中,白付
                                // 一次 cache_key 比较。缓存下沉到了**每个 pane**
                                // (`terminal_area::cached_terminal`):刷屏的那个
                                // 未命中,闲着的照走缓存
                                .child(self.workbench_area.clone()),
                        )
                        .when(terminals_visible && terminal_page_active, |el| {
                            // 同样套缓存:竖条的数据源只有 store。占位样式照抄它
                            // render 根节点的 `.w(WIDTH).h_full().flex_none()`
                            el.child(cached_panel(
                                &self.terminals_panel,
                                StyleRefinement::default()
                                    .w(px(terminals_panel::WIDTH))
                                    .h_full()
                                    .flex_none(),
                            ))
                        })
                        .when(squeeze > 0.0, |el| {
                            el.child(div().flex_none().w(px(squeeze)).h_full())
                        }),
                ),
            )
            .on_resize(move |state, _window, cx| {
                let sizes: Vec<f64> = state
                    .read(cx)
                    .sizes()
                    .iter()
                    .map(|p| f32::from(*p) as f64)
                    .collect();
                store_for_columns.update(cx, |store, cx| {
                    // 折叠/收起的那一栏**不写回**:gpui-component 的
                    // `ResizableState` 按 children 个数占位,不可见的面板既不
                    // 渲染也不上报自己的尺寸,`sizes[i]` 停在建组时的最小值上。
                    // 照抄回去的话「收起中间栏后拖一下右边的分隔条」就把中间栏
                    // 宽度抹成最小值,再展开时只剩一条缝。
                    let mut columns = store
                        .config()
                        .layout_sizes
                        .clone()
                        .filter(|s| s.len() == 2)
                        .unwrap_or_else(|| DEFAULT_COLUMNS.to_vec());
                    if middle_visible && let Some(w) = sizes.first() {
                        columns[0] = *w;
                    }
                    if let Some(w) = sizes.get(1) {
                        columns[1] = *w;
                    }
                    // layoutSizes 恒为两项 —— 磁盘格式与装机版共用,不许长出第三项
                    store.set_layout_sizes(columns, cx);
                });
            });

        let toggle_strip =
            self.render_activity_bar(middle_visible, global_status, unread, update_shown, cx);
        let drawer_layer = self.render_right_drawer(
            drawer_panel,
            drawer_width,
            detail_pane,
            detail_width,
            window,
            cx,
        );
        let usage_layer = self.render_usage_layer(window, cx);
        // 三栏 + 悬浮抽屉的那一层。标题栏之下的**全部**内容都在这里 ——
        // 原版同款(`App.tsx:478` 的 `flex-1 overflow-hidden flex`),抽屉的
        // `absolute` 于是不会盖到标题栏上。用量面板**不在这里**:它是 Modal
        // (原版 fixed inset-0,遮罩要盖住标题栏),挂根层与 Dialog 族同构。
        let body = div()
            .flex_1()
            .overflow_hidden()
            .relative()
            .flex()
            // Activity Bar 的 flex 占位仍是 44px;视觉条本体在 columns 后面以
            // absolute sibling 画,让右伸的标签不被 columns 覆盖。
            .child(div().flex_none().w(px(activity_bar::WIDTH)).h_full())
            .child(columns_host().child(columns_group))
            // 中栏收起的出场层(盖在终端区左段上滑走,边条画在它之上)
            .children(middle_exit)
            .child(toggle_strip)
            .children(drawer_layer)
            // 自建 toast 层。挂在 `body`(它是 `relative`)里而不是根上 ——
            // 原版 `.toast-stack` 贴的是视口右下角(`fixed right:16 bottom:16`),
            // 标题栏在上面本来就碰不到它;挂进 body 后底边就是窗口底边,等价。
            // 排在抽屉与用量面板**之后** = 画在它们之上,对应原版 `z-index:70`
            // (浮层 50 / 分隔条 35)。
            //
            // S 批记档的「gpui-component 是右上角起堆」这条差异到此为止:自建层
            // 按原版右下角起堆。`render_notification_layer` 留着不动(组件库内部
            // 别处可能还用),只是 mt-app 不再往它里面推东西。
            .child(self.toast_layer.clone())
            .children(Root::render_notification_layer(window, cx));

        div()
            .size_full()
            .relative()
            .flex()
            // 标题栏是根 flex-col 的**首个** child(`App.tsx:474-478`)。
            // ⚠️ 它**不受配置加载失败门控** —— 配置读不出来时用户也得有地方把
            // 窗口关掉(原版那句原注释)。GPUI 侧配置目录不可用时压根开不出窗口
            // (`main()` 里直接 return),这条门控在这边只剩语义上的对齐。
            .flex_col()
            // 窗口底色 + 每帧合成对账:绕开 HTML 预览 WebView 挖的洞画,必须是
            // **第一个**子节点(prepaint 最先跑、paint 垫在最底下,见 `native_view`)
            .child(native_view::root_bg(ui::bg_base()))
            .text_color(ui::text_primary())
            // 界面字族(`config.uiFontFamily`)。gpui 的 `font_family` 会**继承**给
            // 所有没自己设过字族的子元素 —— 等价于原版把它写进 `--app-font-family`
            // 这个 CSS 变量,一处替换全局跟随。字号那一路走 `ui::font_px`。
            .when_some(ui::ui_font_family(), |el, family| el.font_family(family))
            // 主题包背景图:**窗口级**铺一张,与原版挂 `#root` 同位置 ——
            // 三栏都透着同一张图(面板底色带 surface_opacity、终端「默认背景不发
            // quad」,两条一起让图透上来)。
            //
            // ⚠️ 与 `TerminalView::set_background_art` 的逐终端那一路**二选一**:
            // 同时开等于同一块像素画两遍图、两层纱罩把 dim 平方。逐终端那路
            // 从没接过线(`pane.rs` 不调 `set_background_art`),这里是唯一一处。
            // 同样绕开 WebView 的洞画。
            .when_some(background, |el, art| el.child(native_view::hole_art(art)))
            // 窗口级选区丢了 MouseUp 的兜底(见该函数注释)。不登记 hitbox,不挡点击
            .child(text_selection_release_guard())
            .key_context("Workspace")
            // Esc 中途取消**任何**内部拖拽(pane 拖拽 / 文件树行拖向终端或树内移动 /
            // 项目列表排序)。原版 `fileDragState.ts` / `paneDragState.ts` 各挂一句
            // `window.addEventListener('keydown', …, true)`,这里合成一处。
            //
            // **必须挂在根、且是捕获相**:按键沿「根 → 焦点节点」下行,焦点在终端上时
            // `TerminalView` 会把 Esc 翻成 `\x1b` 写进 PTY 并 `stop_propagation`,
            // 冒泡相收不到;而从文件树起拖时焦点在**文件树行**上(按下即聚焦),
            // 终端区那一层压根不在派发路径上 —— 挂终端区根容器只对 pane 拖拽有效
            // (此前正是那样,记档在 `dnd` 模块注释)。
            //
            // 只在**真有拖拽在飞**时吞掉这次 Esc,没拖拽时照常放行,终端里按 Esc
            // 的行为一个字节都不变。各视图的落点残留(高亮/档位)不在这儿清:它们
            // 都与 `cx.has_active_drag()` 与门、并在自己的 render 里对账,
            // `stop_active_drag` 自带一次 `window.refresh()` 就够了。资源管理器拖进来的
            // `ExternalPaths` 不经这里:OLE 拖拽期间 Esc 由拖源(Explorer)处理,
            // gpui 收到 `FileDropEvent::Exited` 时自己清 active_drag。
            .capture_key_down(|event: &gpui::KeyDownEvent, window, cx| {
                if event.keystroke.key != "escape" || !cx.has_active_drag() {
                    return;
                }
                cx.stop_active_drag(window);
                cx.stop_propagation();
            })
            .on_action(cx.listener(Self::on_new_terminal))
            .on_action(cx.listener(Self::on_close_pane))
            .on_action(cx.listener(Self::on_split_right))
            .on_action(cx.listener(Self::on_split_down))
            .on_action(cx.listener(Self::on_next_pane))
            .on_action(cx.listener(Self::on_prev_pane))
            .on_action(cx.listener(Self::on_select_pane))
            .on_action(cx.listener(Self::on_toggle_middle))
            .on_action(cx.listener(Self::on_rename_pane))
            .on_action(cx.listener(Self::on_open_settings))
            .on_action(cx.listener(Self::on_toggle_sessions))
            .on_action(cx.listener(Self::on_toggle_usage))
            .on_action(cx.listener(Self::on_jump_attention))
            .on_action(cx.listener(Self::on_focus_left))
            .on_action(cx.listener(Self::on_focus_right))
            .on_action(cx.listener(Self::on_focus_up))
            .on_action(cx.listener(Self::on_focus_down))
            .on_action(cx.listener(Self::on_terminal_search))
            .on_action(cx.listener(Self::on_global_search))
            .on_action(cx.listener(Self::on_quick_open))
            .on_action(cx.listener(Self::on_switch_project))
            .on_action(cx.listener(Self::on_marker_prev))
            .on_action(cx.listener(Self::on_marker_next))
            .on_action(cx.listener(Self::on_toggle_command_library))
            .on_action(cx.listener(Self::on_toggle_workbench_page))
            .on_action(cx.listener(Self::on_close_terminal_tab))
            .on_action(cx.listener(Self::on_toggle_maximize_pane))
            .on_action(cx.listener(Self::on_move_tab_left))
            .on_action(cx.listener(Self::on_move_tab_right))
            .on_action(cx.listener(Self::on_resize_split_left))
            .on_action(cx.listener(Self::on_resize_split_right))
            .on_action(cx.listener(Self::on_resize_split_up))
            .on_action(cx.listener(Self::on_resize_split_down))
            .on_action(cx.listener(Self::on_reopen_closed_terminal))
            // 拖拽期间鼠标可能划出手柄(甚至划过终端),所以移动/松手挂在**根**上
            // —— 等价于原版往 document 上挂 mousemove/mouseup
            .when(self.drawer_drag.is_some(), |el| {
                el.on_mouse_move(cx.listener(
                    |this: &mut Self, event: &gpui::MouseMoveEvent, _window, cx| {
                        if let Some(drag) = this.drawer_drag.as_mut() {
                            let delta = f32::from(drag.start_x - event.position.x) as f64;
                            let width = (drag.start_width + delta).clamp(240.0, 720.0);
                            // 宽度真变了才重画:钳到上下限之后鼠标接着动,每一下都是一次
                            // 白画的整窗帧
                            if width != drag.width {
                                drag.width = width;
                                cx.notify();
                            }
                        }
                    },
                ))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(
                        |this: &mut Self, _event: &gpui::MouseUpEvent, _window, cx| {
                            let Some(drag) = this.drawer_drag.take() else {
                                return;
                            };
                            this.store.update(cx, |store, cx| {
                                store.set_right_drawer_width(drag.width, cx)
                            });
                            cx.notify();
                        },
                    ),
                )
            })
            // 提交详情栏左缘的拖拽:同一套,栏也贴在右侧(抽屉左边),往左拖 = 变宽
            .when(self.detail_drag.is_some(), |el| {
                el.on_mouse_move(cx.listener(
                    |this: &mut Self, event: &gpui::MouseMoveEvent, _window, cx| {
                        if let Some(drag) = this.detail_drag.as_mut() {
                            let delta = f32::from(drag.start_x - event.position.x) as f64;
                            let width = (drag.start_width + delta)
                                .clamp(store::GIT_DETAIL_MIN_WIDTH, store::GIT_DETAIL_MAX_WIDTH);
                            // 同上:钳住之后不再每次移动都整窗重画
                            if width != drag.width {
                                drag.width = width;
                                cx.notify();
                            }
                        }
                    },
                ))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(
                        |this: &mut Self, _event: &gpui::MouseUpEvent, _window, cx| {
                            let Some(drag) = this.detail_drag.take() else {
                                return;
                            };
                            this.store
                                .update(cx, |store, cx| store.set_git_detail_width(drag.width, cx));
                            cx.notify();
                        },
                    ),
                )
            })
            // ⚠️ 标题栏**不套** [`cached_panel`]:它靠 `.window_control_area(..)`
            // 在 paint 期登记拖拽区与三键的 hitbox,而 `window_control_hitboxes`
            // 不在 `PaintIndex` 里、`reuse_paint` 不搬它,每帧还被 `Frame::clear`
            // 清空 —— 命中一次缓存,那一帧的窗口拖拽与最小化/最大化/关闭就全没了。
            // 它每帧那两遍全 pane 扫改走 `store.title_bar_snapshot()` 合成一次遍历。
            .child(self.title_bar.clone())
            // 弹窗开着时背景里的字不参与选区(见 `dialog_host::confine_background`)
            .child(dialog_host::confine_background(body, window, cx))
            // 弹窗毛玻璃背板:垫在用量面板与 Dialog 层之下、其余一切之上
            // (含标题栏 —— 原版 Modal 是 `fixed inset-0`,遮罩同样盖住标题栏)。
            // 压暗不在这层:Dialog 走自己的 `cx.theme().overlay`(black/50,
            // theme.rs::apply 钉的),用量面板走 usage_layer 自己的 black/50 ——
            // 两族弹窗共用同一张玻璃,观感一致(用户实测口径)。浓度随遮罩淡入
            // (`frost_alpha`),全开静止时是 1;收起那一刻撤掉。
            .children(self.frost.clone().filter(|_| frost_wanted).map(|img| {
                div()
                    .absolute()
                    .inset_0()
                    .opacity(frost_alpha)
                    .child(gpui::img(img).size_full().object_fit(gpui::ObjectFit::Fill))
            }))
            // 用量统计(自绘 Modal):玻璃之上、Dialog 层之下 —— Dialog 叠开时
            // (如面板里再弹确认框)压在它上面,与原版 Modal 叠 Modal 同序。
            .children(usage_layer)
            // 弹窗层(自建外壳 `dialog_host`,栈按窗口存),由应用视图**画出来**。
            // (它自己走 `anchored()` 定在视口中央,挂哪一层都一样;
            //  通知层不同 —— 见 `body` 那边的注释。)
            .children(dialog_host::render_layer(window, cx))
            // 右键菜单层。零尺寸的绝对定位壳子 —— 菜单自己走 anchored(窗口坐标)
            // + deferred,不参与这里的 flex 布局,收着的时候一个像素都不占。
            .child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w_0()
                    .h_0()
                    .child(self.menu_layer.clone())
                    // 离场托管层:宿主已经丢掉、退场还没播完的浮层(日期选择器 / 图片放大)
                    // 在这里画完最后几帧。它们同样是 deferred + 窗口坐标,零尺寸壳子不碍事
                    .child(overlay_motion::exit_layer(cx)),
            )
    }
}
