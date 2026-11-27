//! 命令库浮层本体:搜索框 + 分组列表 + 底部两颗按钮。
//!
//! 宿主是 `terminal_area`:它负责「从哪颗按钮弹、遮罩、点外面关、焦点还原」
//! (与 AI 标记浮层同一套),以及框体(底色 / 边框 / 阴影)与开合动画(`crate::popover_reveal`);
//! 本实体只画面板内容、管键盘游标与行内状态。**跨次打开
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
//!
//! # 分组折叠的动画
//!
//! 点分组标题:箭头转过去(`fold_motion`),组里那几行高度裁剪展开 / 收起
//! (`height_reveal`,收起时照旧画着那几行、收完才摘)。浮层框体的开合动画由宿主按内容的
//! 自然尺寸逐帧插值,两者叠在一起框体会追着一个变化的目标走 —— 所以**浮层打开后
//! [`crate::popover_reveal::REVEAL_OPEN`] 之内点的那一下不播高度**,直接到位
//! ([`fold_animates`];本实体看不到宿主的开合状态,按打开时刻推算)。每次打开都清掉
//! 上一次残留的折叠动画。
//!
//! 每个分组**常驻**一份 `HeightReveal`(展开静止时只是一层不设高度的外壳 + 一块量高度的
//! 探针):收起要从「眼下的自然高度」起收,那个高度得在点之前就量好 —— 临点时才建的话
//! 第一帧不知道多高,会先塌成 0 再弹回来。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use gpui::{
    App, AppContext, Bounds, ClickEvent, ClipboardItem, Context, Entity, EventEmitter,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Pixels, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Window, actions, canvas,
    div, point, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::SavedCommand;
use mt_ui::icons::VectorIcon;
use mt_ui::tooltip::TooltipExt as _;

use crate::fold_motion::{FlipTweens, OPEN_TURNS};
use crate::height_reveal::HeightReveal;
use crate::i18n::t;
use crate::store::AppStore;
use crate::ui;

use super::{
    ICON_CHEVRON_RIGHT, ICON_CLOSE, ICON_COPY, ICON_EDIT, ICON_FOLDER_PLUS, ICON_PLAY, ICON_PLUS,
    ICON_TRASH, VisibleBucket, flatten_rows, visible_buckets,
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

/// 这一下折叠要不要播高度动画:浮层打开(`opened`)满 [`crate::popover_reveal::REVEAL_OPEN`]
/// 之后才播 —— 那时宿主的开合动画已落定(见模块注释「分组折叠的动画」)。
fn fold_animates(opened: Instant, now: Instant) -> bool {
    now.saturating_duration_since(opened) >= crate::popover_reveal::REVEAL_OPEN
}

/// ↑/↓ 挪到的那一行要滚进列表视口:返回新的纵向滚动偏移(gpui 口径:`y ≤ 0`,越往下
/// 越负);整行已经可见、或者已经滚到头滚不动了,返回 `None`。
///
/// 坐标都是窗口坐标、按**这一帧**的偏移排好的:`row_*` 是那一行的上下沿,`view_*` 是列表
/// 视口的上下沿,`max_scroll` 是最多能往下滚多远(≥ 0)。策略与 gpui `scroll_to_item` 的
/// 默认一致:露不全就滚最少的那一段;行比视口还高时顶对齐。
///
/// 不能直接用 `scroll_to_item`:它按滚动区的**直接子元素**数,而这里的行有的套在分组的
/// 折叠件(`HeightReveal`)里,分组标题也占位,下标对不上行。
fn reveal_scroll_y(
    offset_y: f32,
    (row_top, row_bottom): (f32, f32),
    (view_top, view_bottom): (f32, f32),
    max_scroll: f32,
) -> Option<f32> {
    // 视口还没排过版(高度 0):量不准,不动
    if view_bottom <= view_top {
        return None;
    }
    let target = if row_top < view_top || row_bottom - row_top > view_bottom - view_top {
        offset_y + (view_top - row_top)
    } else if row_bottom > view_bottom {
        offset_y - (row_bottom - view_bottom)
    } else {
        return None;
    };
    let target = target.clamp(-max_scroll.max(0.0), 0.0);
    // 夹到头之后与眼下没差别 = 滚不动了。必须收手:探针每帧都在,不收就是一个要帧的死循环
    ((target - offset_y).abs() > 0.5).then_some(target)
}

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
    /// 列表的滚动位置。↑/↓ 挪游标时把那一行滚进视口(见 [`Self::cursor_reveal_probe`])。
    list_scroll: ScrollHandle,
    /// 游标刚被键盘挪过、还没确认滚进视口。鼠标悬停挪游标不置它 —— 悬停那一行本来就在
    /// 眼前,跟着滚只会把列表从鼠标底下拽走。
    reveal_cursor: bool,
    /// 正等二次确认的删除目标(命令 id)。
    pending_delete: Option<String>,
    /// 刚复制了哪条(短暂显示「已复制」)。
    copied: Option<String>,
    _copied_timer: Option<Task<()>>,
    /// 分组标题的折叠箭头:只让刚点的那一组转(见 `fold_motion`)。
    chevrons: FlipTweens,
    /// 各分组的高度展开 / 收起(键同 `collapsed`,每组常驻一份,见模块注释)。
    folds: HashMap<String, HeightReveal>,
    /// 这一次打开的时刻(折叠动画等宿主开合落定再播,见 [`fold_animates`])。
    opened_at: Instant,
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
                // 关键词一变游标回到第一条,顺带撤掉悬而未决的删除;列表回到顶上
                // (第一条前面可能还有分组标题,只把那一行滚进来会把标题切掉)
                InputEvent::Change => {
                    this.cursor = 0;
                    this.pending_delete = None;
                    this.reveal_cursor = false;
                    this.list_scroll.set_offset(point(px(0.0), px(0.0)));
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
            list_scroll: ScrollHandle::new(),
            reveal_cursor: false,
            pending_delete: None,
            copied: None,
            _copied_timer: None,
            chevrons: FlipTweens::new(mt_ui::motion::CHEVRON_ROTATE),
            folds: HashMap::new(),
            opened_at: Instant::now(),
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
        // 实体跨次打开常驻:上一次滚到的位置不留(游标回到第一条,列表也回到顶上)
        self.reveal_cursor = false;
        self.list_scroll.set_offset(point(px(0.0), px(0.0)));
        self.pending_delete = None;
        self.copied = None;
        self._copied_timer = None;
        // 上一次残留的折叠动画作废:重新打开时各组按状态静止地画
        self.chevrons = FlipTweens::new(mt_ui::motion::CHEVRON_ROTATE);
        for (key, fold) in self.folds.iter_mut() {
            fold.jump(!self.collapsed.contains(key));
        }
        self.opened_at = Instant::now();
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
        // 列表上限 360px,九行上下光标就出视口了:下一帧把游标那一行滚进来
        self.reveal_cursor = true;
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
        let open = self.collapsed.remove(key);
        if !open {
            self.collapsed.insert(key.to_string());
        }
        self.chevrons.flip_open(key, open, OPEN_TURNS);
        // 在飞的那一场从当前高度掉头;浮层还在开合动画里就直接到位(见 [`fold_animates`])
        let animate = fold_animates(self.opened_at, Instant::now());
        let fold = self
            .folds
            .entry(key.to_string())
            .or_insert_with(|| HeightReveal::new(!open));
        if animate {
            fold.set_open(open);
        } else {
            fold.jump(open);
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

    /// 挂在游标那一行上的探针(不画东西的 canvas):prepaint 时拿到这一行与列表视口的窗口
    /// 坐标,露不全就改列表的滚动偏移([`reveal_scroll_y`]),下一帧重画(套路同
    /// `popover_reveal::anchor_probe`:量完不在绘制期 notify,排到下一帧);整行可见即撤掉
    /// [`Self::reveal_cursor`],探针随之不再挂。
    fn cursor_reveal_probe(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let entity = cx.entity();
        let scroll = self.list_scroll.clone();
        canvas(
            move |row: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
                let view = scroll.bounds();
                let offset = scroll.offset();
                let next = reveal_scroll_y(
                    f32::from(offset.y),
                    (f32::from(row.top()), f32::from(row.bottom())),
                    (f32::from(view.top()), f32::from(view.bottom())),
                    f32::from(scroll.max_offset().y),
                );
                let moved = entity.update(cx, |this, _| match next {
                    Some(y) => {
                        scroll.set_offset(point(offset.x, px(y)));
                        true
                    }
                    None => {
                        this.reveal_cursor = false;
                        false
                    }
                });
                if moved {
                    window.on_next_frame(move |_, cx| entity.update(cx, |_, cx| cx.notify()));
                }
            },
            |_, _, _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

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
        // 只画一枚右指箭头、按圈转到下指(见 `fold_motion`)
        let turns = self
            .chevrons
            .value_open(&key, !bucket.collapsed, OPEN_TURNS);
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
                    .child(
                        VectorIcon::new(ICON_CHEVRON_RIGHT, px(11.0))
                            .ink(ui::text_muted())
                            .rotation(turns),
                    ),
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

        // 键盘刚挪到这一行:挂探针把它滚进视口(见 [`Self::cursor_reveal_probe`])
        let reveal = (is_cursor && self.reveal_cursor).then(|| self.cursor_reveal_probe(cx));

        div()
            .id(SharedString::from(format!("cmd-row-{id}")))
            .group("cmd-row")
            // 探针按整行铺(`absolute` + `size_full`),得有个定位锚
            .relative()
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
            .children(reveal)
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let buckets = self.buckets(cx);
        let query = self.query.read(cx).value().to_string();
        let searching = !query.trim().is_empty();
        self.chevrons.drive(window);
        // 每组常驻一份折叠件:新出现的组按状态建成静止的,没了的组丢掉;状态被别处改过
        // (解散分组、搜索中改了库)就直接到位。搜索中折叠不生效,整块不走折叠件
        if !searching {
            self.folds
                .retain(|key, _| buckets.iter().any(|b| b.key() == key.as_str()));
            for bucket in &buckets {
                let open = !bucket.collapsed;
                let fold = self
                    .folds
                    .entry(bucket.key().to_string())
                    .or_insert_with(|| HeightReveal::new(open));
                if fold.is_open() != open {
                    fold.jump(open);
                }
            }
        }
        // 正在收起的组:条目已不在可见桶里,从「全部展开」的那份里取来画最后一程
        let collapsing = !searching
            && self
                .folds
                .values()
                .any(|fold| !fold.is_open() && fold.is_mounted());
        let expanded = if collapsing {
            let lib = self.store.read(cx).command_library();
            visible_buckets(lib, &query, &HashSet::new())
        } else {
            Vec::new()
        };
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
            .track_scroll(&self.list_scroll)
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
                let fold = (!searching).then(|| self.folds.get(bucket.key())).flatten();
                let Some(fold) = fold.filter(|fold| fold.is_mounted()) else {
                    // 搜索中(不走折叠件),或收起且静止(一行都不画)
                    for cmd in &bucket.items {
                        let is_cursor = flat_idx == cursor;
                        list =
                            list.child(self.render_row(cmd, flat_idx, is_cursor, mono.clone(), cx));
                        flat_idx += 1;
                    }
                    continue;
                };
                // 组里那几行装进折叠件(静止展开时只是一层外壳)。展开着的照常进游标序列;
                // 收起中的已不在游标序列里(也接不到鼠标),只画最后一程
                let mut rows = div();
                if fold.is_open() {
                    for cmd in &bucket.items {
                        let is_cursor = flat_idx == cursor;
                        rows =
                            rows.child(self.render_row(cmd, flat_idx, is_cursor, mono.clone(), cx));
                        flat_idx += 1;
                    }
                } else if let Some(full) = expanded.iter().find(|b| b.key() == bucket.key()) {
                    for cmd in &full.items {
                        rows =
                            rows.child(self.render_row(cmd, usize::MAX, false, mono.clone(), cx));
                    }
                }
                list = list.child(fold.render(rows, window));
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
            // 框体(圆角 / 边框 / `bg_overlay` 底色 / 阴影)与拦鼠标都由宿主画
            // (`terminal_area::render_command_popover`,开合动画要按插值矩形画它);
            // 这里只让出边框那 1px,内容位置与框体自带时一致
            .p(px(1.0))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 浮层自己还在开合动画里(打开不满 `REVEAL_OPEN`)时点的那一下不播高度,直接到位 ——
    /// 否则框体会追着一个变化的目标插值。
    #[test]
    fn 浮层开合落定之前点的折叠不播高度() {
        let t0 = Instant::now();
        let open = crate::popover_reveal::REVEAL_OPEN;
        assert!(!fold_animates(t0, t0));
        assert!(!fold_animates(t0, t0 + open / 2));
        assert!(fold_animates(t0, t0 + open));
        assert!(fold_animates(t0, t0 + open * 3));
    }

    /// ↑/↓ 挪游标:整行可见不动;露不全滚最少那一段;行比视口高顶对齐;滚到头就收手。
    #[test]
    fn 游标行滚进视口只滚最少的一段() {
        // 视口 100..460(高 360),最多能往下滚 400
        let view = (100.0, 460.0);
        let max = 400.0;
        assert_eq!(
            reveal_scroll_y(0.0, (200.0, 230.0), view, max),
            None,
            "整行可见:不动"
        );
        assert_eq!(
            reveal_scroll_y(0.0, (440.0, 470.0), view, max),
            Some(-10.0),
            "下沿露出 10px:往下滚 10,行底贴视口底"
        );
        assert_eq!(
            reveal_scroll_y(-120.0, (80.0, 110.0), view, max),
            Some(-100.0),
            "上沿被切掉 20px:往回滚 20,行顶贴视口顶"
        );
        assert_eq!(
            reveal_scroll_y(-50.0, (60.0, 500.0), view, max),
            Some(-10.0),
            "行比视口还高:顶对齐"
        );
    }

    #[test]
    fn 游标行滚到头滚不动就收手() {
        let view = (100.0, 460.0);
        // 已经滚到最底(-400),行还在视口下面(布局与偏移一时没对上):夹完等于没动,不再要帧
        assert_eq!(reveal_scroll_y(-400.0, (470.0, 500.0), view, 400.0), None);
        // 内容不够一屏(max 0):怎么都滚不动
        assert_eq!(reveal_scroll_y(0.0, (470.0, 500.0), view, 0.0), None);
        // 往回滚超过顶:夹到 0
        assert_eq!(reveal_scroll_y(-10.0, (60.0, 90.0), view, 400.0), Some(0.0));
        // 视口还没排过版(高度 0):不动
        assert_eq!(reveal_scroll_y(0.0, (470.0, 500.0), (0.0, 0.0), 400.0), None);
    }
}
