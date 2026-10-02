//! 命令库浮层本体:搜索框 + 分组列表 + 底部两颗按钮。
//!
//! 宿主是 `terminal_area`:它负责「从哪颗按钮弹、遮罩、点外面关、焦点还原」
//! (与 AI 标记浮层同一套),本实体只画面板、管键盘游标与行内状态。**跨次打开
//! 常驻**(宿主懒建一次、反复 [`CommandPopover::open_for`]),所以分组折叠态
//! 能在同一次会话里记住;搜索词每次打开清空。
//!
//! # 键盘
//!
//! 打开即聚焦搜索框。↑/↓ 走 action(`CommandLibraryPrev/Next`,绑在
//! `"CommandLibrary > Input"` 上 —— 为什么必须与 `Input` 同深度见
//! `project_switcher` 模块注释),↵ 订阅 `InputEvent::PressEnter`,Ctrl+↵ 也是
//! action(`CommandLibraryPasteOnly`)。Esc 走容器的 `on_key_down`:单行输入框
//! 的 escape 处理器以 `cx.propagate()` 收尾,键会冒到这里。
//!
//! # 删除不弹确认框
//!
//! 浮层里再叠一个 Dialog 太重(遮罩叠遮罩),改成**行内二次确认**:点 🗑 后该行
//! 的动作区换成「确认删除 / 取消」,再点一下才真删;点别的行、Esc、关浮层都算取消。

use std::collections::HashSet;
use std::time::Duration;

use gpui::{
    App, AppContext, ClickEvent, ClipboardItem, Context, Entity, EventEmitter, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Task, Window, actions, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::SavedCommand;
use mt_ui::icons::VectorIcon;
use mt_ui::tooltip::TooltipExt as _;

use crate::i18n::t;
use crate::store::AppStore;
use crate::ui;

use super::{
    ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT, ICON_CLOSE, ICON_COPY, ICON_EDIT, ICON_FOLDER_PLUS,
    ICON_PLAY, ICON_PLUS, ICON_TRASH, VisibleBucket, flatten_rows, visible_buckets,
};

actions!(
    mini_term,
    [
        /// 上一条(↑)。绑定在 `"CommandLibrary > Input"` 上。
        CommandLibraryPrev,
        /// 下一条(↓)。
        CommandLibraryNext,
        /// 只把命令敲进终端、不回车(Ctrl+↵)。
        CommandLibraryPasteOnly,
    ]
);

/// 面板宽度 / 列表最大高度(13px 基准,用时过 `ui::font_px`)。
pub const PANEL_WIDTH: f32 = 372.0;
const LIST_MAX_HEIGHT: f32 = 360.0;
/// 「已复制」提示停留多久。
const COPIED_MS: u64 = 1200;
/// 行内小图标钮的边长 / 图标尺寸。
const ACT_BTN: f32 = 20.0;
const ACT_ICON: f32 = 12.0;

/// 浮层要宿主替它做的事。宿主先关浮层(还焦点),再开对应弹窗 —— 顺序反了
/// Dialog 抢到的焦点会被焦点还原覆盖掉。
#[derive(Debug, Clone)]
pub enum PopoverEvent {
    /// 收起(Esc / 跑完一条)。
    Close,
    /// 开「新增 / 编辑命令」弹窗。
    Edit {
        existing: Option<SavedCommand>,
        group: Option<String>,
    },
    /// 开「新建分组」输入框。
    NewGroup,
    /// 开「重命名分组」输入框。
    RenameGroup(String),
}

pub struct CommandPopover {
    store: Entity<AppStore>,
    /// 命令要写进哪个 pane:`(project_id, pane_id)`。宿主每次打开时设。
    target: Option<(String, String)>,
    query: Entity<InputState>,
    /// 折叠着的分组(键见 [`super::UNGROUPED_KEY`])。跨次打开保留。
    collapsed: HashSet<String>,
    /// 键盘游标:可见行(折叠桶不算)里的第几条。
    cursor: usize,
    /// 正等二次确认的删除目标(命令 id)。
    pending_delete: Option<String>,
    /// 刚复制了哪条(短暂显示「已复制」)。
    copied: Option<String>,
    _copied_timer: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PopoverEvent> for CommandPopover {}

impl CommandPopover {
    pub fn new(store: Entity<AppStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| {
            InputState::new(window, cx).placeholder(t("commandLibrary", "searchPlaceholder"))
        });
        let sub = cx.subscribe_in(
            &query,
            window,
            |this: &mut Self, _, event: &InputEvent, window, cx| match event {
                // 关键词一变游标回到第一条,顺带撤掉悬而未决的删除
                InputEvent::Change => {
                    this.cursor = 0;
                    this.pending_delete = None;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } => this.run_cursor(true, window, cx),
                _ => {}
            },
        );
        Self {
            store,
            target: None,
            query,
            collapsed: HashSet::new(),
            cursor: 0,
            pending_delete: None,
            copied: None,
            _copied_timer: None,
            _subs: vec![sub],
        }
    }

    /// 宿主打开浮层时调:记下目标 pane、清搜索词与行内状态、聚焦搜索框。
    pub fn open_for(
        &mut self,
        project_id: String,
        pane_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.target = Some((project_id, pane_id));
        self.cursor = 0;
        self.pending_delete = None;
        self.copied = None;
        self._copied_timer = None;
        self.query.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    /// 当前可见桶(每帧现算:库是几十条量级,缓存反而要失效通道)。
    fn buckets(&self, cx: &App) -> Vec<VisibleBucket> {
        let lib = self.store.read(cx).command_library();
        let query = self.query.read(cx).value().to_string();
        visible_buckets(lib, &query, &self.collapsed)
    }

    fn move_cursor(&mut self, delta: i32, cx: &mut Context<Self>) {
        let len = flatten_rows(&self.buckets(cx)).len();
        self.cursor = crate::project_switcher::next_cursor(self.cursor, len, delta);
        self.pending_delete = None;
        cx.notify();
    }

    /// 跑游标所在那条。
    fn run_cursor(&mut self, newline: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rows = flatten_rows(&self.buckets(cx));
        let cursor = self.cursor.min(rows.len().saturating_sub(1));
        let Some(cmd) = rows.get(cursor).cloned() else {
            return;
        };
        self.run(&cmd, newline, window, cx);
    }

    /// 把命令写进目标 pane 并收浮层。写不进去(pane 没了)就只收浮层 ——
    /// 留着一个对着空气的浮层没有意义。
    fn run(
        &mut self,
        cmd: &SavedCommand,
        newline: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((project_id, pane_id)) = self.target.clone() {
            let command = cmd.command.clone();
            self.store.update(cx, |store, cx| {
                store.run_saved_command(&project_id, &pane_id, &command, newline, cx);
            });
        }
        cx.emit(PopoverEvent::Close);
    }

    fn copy(&mut self, cmd: &SavedCommand, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(cmd.command.clone()));
        self.copied = Some(cmd.id.clone());
        let id = cmd.id.clone();
        self._copied_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(COPIED_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.copied.as_deref() == Some(id.as_str()) {
                    this.copied = None;
                    cx.notify();
                }
            });
        }));
        cx.notify();
    }

    fn toggle_group(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(key) {
            self.collapsed.insert(key.to_string());
        }
        self.cursor = 0;
        self.pending_delete = None;
        cx.notify();
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if event.keystroke.key != "escape" {
            return;
        }
        cx.stop_propagation();
        // 先撤删除确认,再按一次才关浮层
        if self.pending_delete.take().is_some() {
            cx.notify();
            return;
        }
        cx.emit(PopoverEvent::Close);
    }

    // ─── 渲染 ─────────────────────────────────────────────

    /// 行尾那种 20×20 的小图标钮。
    fn act_button(
        id: SharedString,
        shapes: &'static [mt_ui::icons::Shape],
        ink: gpui::Hsla,
        tip: &'static str,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .flex_none()
            .w(px(ACT_BTN))
            .h(px(ACT_BTN))
            .rounded(px(3.0))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|el| el.bg(ui::border_subtle()))
            .tip(tip)
            .child(VectorIcon::new(shapes, px(ACT_ICON)).ink(ink))
    }

    fn render_group_header(
        &self,
        bucket: &VisibleBucket,
        searching: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let key = bucket.key().to_string();
        let name: SharedString = match &bucket.group {
            Some(g) => g.clone().into(),
            None => t("commandLibrary", "ungrouped").into(),
        };
        let chevron = if bucket.collapsed {
            ICON_CHEVRON_RIGHT
        } else {
            ICON_CHEVRON_DOWN
        };
        let group_for_add = bucket.group.clone();
        let group_for_rename = bucket.group.clone();
        let group_for_dissolve = bucket.group.clone();
        let key_for_toggle = key.clone();

        div()
            .id(SharedString::from(format!("cmd-grp-{key}")))
            .group("cmd-grp")
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(10.0))
            .pt(px(7.0))
            .pb(px(3.0))
            .text_size(ui::font_px(11.0))
            .text_color(ui::text_muted())
            // 搜索中折叠不生效,标题行就不该像个能点的东西
            .when(!searching, |el| {
                el.cursor_pointer().on_click(cx.listener(
                    move |this, _: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        this.toggle_group(&key_for_toggle, cx);
                    },
                ))
            })
            .child(
                div()
                    .flex_none()
                    .w(px(11.0))
                    .h(px(11.0))
                    .when(searching, |el| el.opacity(0.0))
                    .child(VectorIcon::new(chevron, px(11.0)).ink(ui::text_muted())),
            )
            .child(div().flex_1().min_w_0().truncate().child(name))
            // 具名分组的管理动作:往这组里加 / 改名 / 解散,悬停才显形。排在计数
            // **之前**:未分组桶没有这一段,计数才能在两种标题行上都贴右缘对齐
            .when(bucket.group.is_some(), |el| {
                el.child(
                    div()
                        .flex()
                        .flex_none()
                        .gap(px(1.0))
                        .mr(px(4.0))
                        .opacity(0.0)
                        .group_hover("cmd-grp", |el| el.opacity(1.0))
                        .child(
                            Self::act_button(
                                SharedString::from(format!("cmd-grp-add-{key}")),
                                ICON_PLUS,
                                ui::text_secondary(),
                                t("commandLibrary", "addCommand"),
                            )
                            .on_click(cx.listener(
                                move |_this, _: &ClickEvent, _window, cx| {
                                    cx.stop_propagation();
                                    cx.emit(PopoverEvent::Edit {
                                        existing: None,
                                        group: group_for_add.clone(),
                                    });
                                },
                            )),
                        )
                        .child(
                            Self::act_button(
                                SharedString::from(format!("cmd-grp-rename-{key}")),
                                ICON_EDIT,
                                ui::text_secondary(),
                                t("commandLibrary", "renameGroup"),
                            )
                            .on_click(cx.listener(
                                move |_this, _: &ClickEvent, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(g) = group_for_rename.clone() {
                                        cx.emit(PopoverEvent::RenameGroup(g));
                                    }
                                },
                            )),
                        )
                        .child(
                            Self::act_button(
                                SharedString::from(format!("cmd-grp-dissolve-{key}")),
                                ICON_CLOSE,
                                ui::text_secondary(),
                                t("commandLibrary", "dissolveGroup"),
                            )
                            .on_click(cx.listener(
                                move |this, _: &ClickEvent, _window, cx| {
                                    cx.stop_propagation();
                                    if let Some(g) = group_for_dissolve.clone() {
                                        this.store.update(cx, |store, cx| {
                                            store.dissolve_command_group(&g, cx)
                                        });
                                        this.collapsed.remove(&g);
                                        this.cursor = 0;
                                        cx.notify();
                                    }
                                },
                            )),
                        ),
                )
            })
            .child(div().flex_none().child(bucket.total.to_string()))
    }

    fn render_row(
        &self,
        cmd: &SavedCommand,
        idx: usize,
        is_cursor: bool,
        mono: SharedString,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let id = cmd.id.clone();
        let pending = self.pending_delete.as_deref() == Some(id.as_str());
        let copied = self.copied.as_deref() == Some(id.as_str());
        let cmd_run = cmd.clone();
        let cmd_copy = cmd.clone();
        let cmd_edit = cmd.clone();
        let id_del = id.clone();
        let id_del_confirm = id.clone();

        // 动作区三态:等确认 → 「确认删除 / 取消」;刚复制 → 「已复制」;
        // 其余 → 悬停显形的三颗图标钮
        let actions = if pending {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(4.0))
                .child(
                    div()
                        .id(SharedString::from(format!("cmd-del-yes-{id}")))
                        .px(px(6.0))
                        .py(px(1.0))
                        .rounded(px(3.0))
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::color_error())
                        .cursor_pointer()
                        .hover(|el| el.bg(ui::border_subtle()))
                        .child(t("commandLibrary", "confirmDelete"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.pending_delete = None;
                            this.store.update(cx, |store, cx| {
                                store.remove_saved_command(&id_del_confirm, cx)
                            });
                            this.cursor = 0;
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("cmd-del-no-{id}")))
                        .px(px(6.0))
                        .py(px(1.0))
                        .rounded(px(3.0))
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_secondary())
                        .cursor_pointer()
                        .hover(|el| el.bg(ui::border_subtle()))
                        .child(t("commandLibrary", "cancel"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.pending_delete = None;
                            cx.notify();
                        })),
                )
        } else if copied {
            div()
                .flex()
                .flex_none()
                .items_center()
                .px(px(4.0))
                .text_size(ui::font_px(11.0))
                .text_color(ui::color_success())
                .child(t("commandLibrary", "copied"))
        } else {
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(1.0))
                // 原版 `opacity-0 group-hover:opacity-100`;透明时仍吃点击,
                // 但它们都在行尾,误触面积可忽略
                .opacity(0.0)
                .group_hover("cmd-row", |el| el.opacity(1.0))
                .child(
                    Self::act_button(
                        SharedString::from(format!("cmd-copy-{id}")),
                        ICON_COPY,
                        ui::text_secondary(),
                        t("commandLibrary", "copy"),
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.copy(&cmd_copy, cx);
                        },
                    )),
                )
                .child(
                    Self::act_button(
                        SharedString::from(format!("cmd-edit-{id}")),
                        ICON_EDIT,
                        ui::text_secondary(),
                        t("commandLibrary", "edit"),
                    )
                    .on_click(cx.listener(
                        move |_this, _: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            cx.emit(PopoverEvent::Edit {
                                existing: Some(cmd_edit.clone()),
                                group: None,
                            });
                        },
                    )),
                )
                .child(
                    Self::act_button(
                        SharedString::from(format!("cmd-del-{id}")),
                        ICON_TRASH,
                        ui::color_error(),
                        t("commandLibrary", "delete"),
                    )
                    .on_click(cx.listener(
                        move |this, _: &ClickEvent, _window, cx| {
                            cx.stop_propagation();
                            this.pending_delete = Some(id_del.clone());
                            cx.notify();
                        },
                    )),
                )
        };

        div()
            .id(SharedString::from(format!("cmd-row-{id}")))
            .group("cmd-row")
            .flex()
            .items_center()
            .gap(px(8.0))
            .pl(px(12.0))
            .pr(px(8.0))
            .py(px(5.0))
            .cursor_pointer()
            .when(is_cursor, |el| el.bg(ui::accent_subtle()))
            // 面板底色就是 `bg_overlay`,悬停再刷它等于没刷,改用 `border_subtle`(同标题栏下拉)
            .when(!is_cursor, |el| el.hover(|el| el.bg(ui::border_subtle())))
            // 行上**不挂** tooltip:它跟着鼠标弹在行中间,正好盖住行尾那三颗
            // 图标钮(真机截到过);「↵ 运行 · Ctrl+↵ 只粘贴」的说明由底栏常驻承担
            // 悬停把游标挪过来(与项目切换器同款):高亮只有一处,不会出现
            // 「键盘选中一行、鼠标悬停另一行」两处亮
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                if *hovered && this.cursor != idx {
                    this.cursor = idx;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                cx.stop_propagation();
                // Ctrl+点击 = 只粘贴不回车(与 Ctrl+↵ 同义)
                let paste_only = event.modifiers().control || event.modifiers().platform;
                this.run(&cmd_run, !paste_only, window, cx);
            }))
            .child(div().flex_none().w(px(14.0)).h(px(14.0)).child(
                VectorIcon::new(ICON_PLAY, px(14.0)).ink(if is_cursor {
                    ui::accent()
                } else {
                    ui::text_muted()
                }),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .truncate()
                            .text_size(ui::font_px(12.5))
                            .text_color(ui::text_primary())
                            .child(SharedString::from(cmd.name.clone())),
                    )
                    .child(
                        div()
                            .truncate()
                            // 与终端同一字族:`"monospace"` 这种泛型族名在 Windows 的
                            // DirectWrite 下解析不到,会静默回落成界面字体
                            .font_family(mono)
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(SharedString::from(cmd.command.clone())),
                    ),
            )
            .child(actions)
    }

    fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let text_btn =
            |id: &'static str, shapes: &'static [mt_ui::icons::Shape], label: &'static str| {
                div()
                    .id(id)
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(6.0))
                    .py(px(3.0))
                    .rounded(px(4.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_secondary())
                    .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
                    .child(VectorIcon::new(shapes, px(12.0)).ink(ui::text_secondary()))
                    .child(label)
            };
        div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(6.0))
            .py(px(5.0))
            .border_t_1()
            .border_color(ui::border_subtle())
            .child(
                text_btn("cmd-add", ICON_PLUS, t("commandLibrary", "addCommand")).on_click(
                    cx.listener(|_this, _: &ClickEvent, _window, cx| {
                        cx.stop_propagation();
                        cx.emit(PopoverEvent::Edit {
                            existing: None,
                            group: None,
                        });
                    }),
                ),
            )
            .child(
                text_btn(
                    "cmd-new-group",
                    ICON_FOLDER_PLUS,
                    t("commandLibrary", "newGroup"),
                )
                .on_click(cx.listener(|_this, _: &ClickEvent, _window, cx| {
                    cx.stop_propagation();
                    cx.emit(PopoverEvent::NewGroup);
                })),
            )
            .child(
                div()
                    .ml_auto()
                    .pr(px(4.0))
                    .text_size(ui::font_px(10.5))
                    .text_color(ui::text_muted())
                    .whitespace_nowrap()
                    .child(t("commandLibrary", "footerHint")),
            )
    }
}

impl Render for CommandPopover {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let buckets = self.buckets(cx);
        let query = self.query.read(cx).value().to_string();
        let searching = !query.trim().is_empty();
        let rows = flatten_rows(&buckets);
        let cursor = self.cursor.min(rows.len().saturating_sub(1));
        let lib_empty = self.store.read(cx).command_library().is_empty();
        let mono = crate::pane_preview::preview_style(self.store.read(cx)).font_family;
        // 只有一个「未分组」桶时不画标题行 —— 全是顶层命令就该是一张平铺列表
        let show_ungrouped_header = buckets.len() > 1;

        let mut list = div()
            .id("cmd-list")
            .w_full()
            .max_h(ui::font_px(LIST_MAX_HEIGHT))
            .overflow_y_scroll()
            .py(px(4.0));

        if lib_empty {
            list = list.child(
                div()
                    .px(px(16.0))
                    .py(px(20.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_size(ui::font_px(12.0))
                            .text_color(ui::text_secondary())
                            .child(t("commandLibrary", "empty")),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .text_center()
                            .child(t("commandLibrary", "emptyHint")),
                    ),
            );
        } else if buckets.is_empty() {
            list = list.child(
                div()
                    .px(px(16.0))
                    .py(px(16.0))
                    .text_center()
                    .text_size(ui::font_px(12.0))
                    .text_color(ui::text_muted())
                    .child(t("commandLibrary", "noMatch")),
            );
        } else {
            let mut flat_idx = 0usize;
            for bucket in &buckets {
                if bucket.group.is_some() || show_ungrouped_header {
                    list = list.child(self.render_group_header(bucket, searching, cx));
                }
                for cmd in &bucket.items {
                    let is_cursor = flat_idx == cursor;
                    list = list.child(self.render_row(cmd, flat_idx, is_cursor, mono.clone(), cx));
                    flat_idx += 1;
                }
            }
        }

        div()
            // 方向键 / Ctrl+↵ 的上下文锚点。键位表在 `hotkeys.rs`,谓词是
            // `"CommandLibrary > Input"`
            .key_context("CommandLibrary")
            .on_action(cx.listener(|this, _: &CommandLibraryPrev, _window, cx| {
                this.move_cursor(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &CommandLibraryNext, _window, cx| {
                this.move_cursor(1, cx);
            }))
            .on_action(
                cx.listener(|this, _: &CommandLibraryPasteOnly, window, cx| {
                    this.run_cursor(false, window, cx);
                }),
            )
            .on_key_down(cx.listener(Self::on_key_down))
            .w(ui::font_px(PANEL_WIDTH))
            .flex()
            .flex_col()
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_subtle())
            // 浮层用 `bg_overlay`(主题包下恒不透明),不用 `bg_elevated` —— 后者会乘
            // `surface_opacity`,面板叠在终端上时底下的字会透出来。与标题栏项目下拉同口径
            .bg(ui::bg_overlay())
            .shadow_lg()
            // 面板内的按下不算「点外」—— 宿主遮罩的 on_mouse_down 靠 hitbox 判定
            .occlude()
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(8.0))
                    .pb(px(4.0))
                    .child(Input::new(&self.query).cleanable(false)),
            )
            .child(list)
            .child(self.render_footer(cx))
    }
}
