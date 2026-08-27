//! 「SSH 连接」面板(对照 `src/components/SshModal.tsx` 777 行)。
//!
//! 左栏分组列表 + 右栏连接列表:连接的增删改、分组的新建/改名/解散、把连接
//! 拖进分组。数据层全在 [`crate::store::AppStore`] 的 SSH 段(每一步立即落盘),
//! 分组归类走 [`crate::ssh_conn`] 的纯函数 —— 「一处实现三处用」是原版的刻意
//! 安排,三个弹窗(本面板 / [`crate::ssh_assoc`] / [`crate::remote_project`])
//! 共用同一份桶顺序与空组处理。
//!
//! # 本模块也是另两个弹窗的**共享视图件**出处
//!
//! [`GroupKey`] / [`resolve_active`] / [`visible_buckets`] / [`sidebar_row`] /
//! [`bucket_header`] / [`conn_card`] 六件都是 `pub(crate)`:三个弹窗的
//! 「左栏 + 右栏桶」是同构的,原版靠 `import { GroupSidebarRow } from './SshModal'`
//! 共用,这里照办。
//!
//! 右栏那整套「分组折叠 + 逐条渲染」也在这儿([`render_conn_buckets`] +
//! [`BucketCollapse`])—— 此前三个弹窗各抄一份,连折叠开关的闭包都一字不差;
//! 行内容(单选圆点 / 勾选框 / 行内表单)由闭包注入,那才是三家真正的差别。
//! 底栏外壳 [`panel_footer`] 同理,多加一个消费方 [`crate::env_vars`]。
//!
//! # 与原版的三处形态差异
//!
//! 1. **拖拽走 gpui 的 `on_drag`/`on_drop`**,不是原版那套 mousedown/mousemove
//!    自研脚手架 —— 那套是 WebView2 `dragDropEnabled` 吃掉 HTML5 DnD 的产物,
//!    gpui 侧不存在该约束(见 [`crate::dnd`] 模块注释)。
//! 2. **「分组」输入框的下拉**(原版 `GroupCombobox`)改成右侧一颗 `▾` 按钮弹
//!    [`crate::menu`] —— 输入框失焦/`onMouseDown preventDefault` 那套在 gpui 里
//!    没有等价物,而菜单是本壳通用的下拉形态(与设置面板同款)。手输新组名照旧。
//! 3. **私钥「…」按钮调 `cx.prompt_for_paths`**(原版 `@tauri-apps/plugin-dialog`)。
//!
//! # 本面板独有:点连接名复制名字
//!
//! [`copyable_name`] —— 只长在本面板的行上。另两个弹窗共用 [`conn_text`],
//! 它们的**整行**点击是勾选 / 单选,名字上再接一个点击语义就打架了。
//!
//! # 防叠开
//!
//! [`crate::overlay::kind::SSH_PANEL`]。删除连接的确认框是**另一种类**
//! (`CONFIRM`),照样叠得上去,Esc 只关它 —— 与原版 `overlayStack` 同语义。

use std::collections::HashSet;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    MouseButton, MouseDownEvent, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_config::SshConnection;

use crate::i18n::{t, tr};
use crate::menu::{self, MenuItem};
use crate::prompt::{Confirm, autofocus, kind, open_guarded};
use crate::ssh_conn::{SshGroupBucket, build_group_buckets, connection_summary};
use crate::store::AppStore;
use crate::ui;

/// 面板宽度(原版 `w-[720px]`)。
pub(crate) const PANEL_W: f32 = 720.0;
/// 面板总高:原版 `h-[70vh] max-h-[680px]`,三个 720 宽面板同款。
/// gpui 没有视口单位,按视口现算 —— Dialog 的 builder 每帧重跑,
/// 拖窗口改大小时跟着变;正文吃掉头/底栏之外的剩余(flex-1),与原版同构。
pub(crate) fn panel_total_h(viewport: gpui::Size<gpui::Pixels>) -> gpui::Pixels {
    (viewport.height * 0.70).min(px(680.0))
}
/// 左栏宽度(原版 `w-44` = 176px)。
const SIDEBAR_W: f32 = 176.0;

// ─── 左栏选中态(三个弹窗共用) ────────────────────────────────

/// 左栏选中的是哪一档。原版是 `string | null`:`null` = 全部、`''` = 未分组、
/// 其余 = 具名分组名(组名已 trim,不会是空串)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum GroupKey {
    #[default]
    All,
    Ungrouped,
    Named(String),
}

/// 选中的分组可能因(在另一个弹窗里)删除 / 重命名 / 解散而消失 —— 回落「全部」。
///
/// `ungrouped_visible` 是「未分组那一行现在画不画得出来」:本面板拖拽期间即便
/// 空桶也照画(好让用户把连接拖出分组),另两个弹窗只看桶空不空。
pub(crate) fn resolve_active(
    selected: &GroupKey,
    group_names: &[String],
    ungrouped_visible: bool,
) -> GroupKey {
    match selected {
        GroupKey::All => GroupKey::All,
        GroupKey::Ungrouped => {
            if ungrouped_visible {
                GroupKey::Ungrouped
            } else {
                GroupKey::All
            }
        }
        GroupKey::Named(name) => {
            if group_names.iter().any(|g| g == name) {
                GroupKey::Named(name.clone())
            } else {
                GroupKey::All
            }
        }
    }
}

/// 右栏要展示的桶:「全部」视图展示所有桶(带可折叠标题),选中某组只展示该桶。
pub(crate) fn visible_buckets(order: &[SshGroupBucket], active: &GroupKey) -> Vec<SshGroupBucket> {
    match active {
        GroupKey::All => order.to_vec(),
        GroupKey::Ungrouped => order.iter().filter(|b| b.group.is_none()).cloned().collect(),
        GroupKey::Named(name) => order
            .iter()
            .filter(|b| b.group.as_deref() == Some(name.as_str()))
            .cloned()
            .collect(),
    }
}

/// 折叠键:具名组用组名,未分组桶用空串(与原版 `bucket.group ?? ''` 同)。
pub(crate) fn bucket_key(bucket: &SshGroupBucket) -> String {
    bucket.group.clone().unwrap_or_default()
}

// ─── 共享小件 ─────────────────────────────────────────────────

/// 左栏的一行(原版导出的 `GroupSidebarRow`)。返回 `Stateful<Div>`,
/// 调用方自己挂 `on_click` / 右键 / 拖放。
pub(crate) fn sidebar_row(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    count: usize,
    active: bool,
    drop_active: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(8.0))
        .mx(px(8.0))
        .px(px(12.0))
        .py(px(6.0))
        .rounded(px(4.0))
        .cursor_pointer()
        .text_size(ui::font_px(13.0))
        .when(active, |el| {
            el.bg(ui::bg_overlay()).text_color(ui::text_primary())
        })
        .when(!active, |el| {
            el.text_color(ui::text_secondary())
                .hover(|el| el.bg(ui::bg_elevated()).text_color(ui::text_primary()))
        })
        // 原版落点高亮是 `outline outline-dashed outline-accent`;gpui 没有
        // outline,用同色虚线边框(1px)代偿 —— 边框会挤 1px 布局,所以**恒占位**
        .border_1()
        .border_dashed()
        .border_color(if drop_active {
            ui::accent()
        } else {
            ui::with_alpha(ui::accent(), 0.0)
        })
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .truncate()
                .child(label.into()),
        )
        .child(
            div()
                .flex_none()
                .text_size(ui::font_px(10.0))
                .text_color(ui::text_muted())
                .child(count.to_string()),
        )
}

/// 右栏桶的可折叠标题(`▸/▾ 组名 (n)`)。
pub(crate) fn bucket_header(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    count: usize,
    collapsed: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w_full()
        .flex()
        .items_center()
        .gap(px(6.0))
        .cursor_pointer()
        .text_size(ui::font_px(11.0))
        .text_color(ui::text_muted())
        .hover(|el| el.text_color(ui::text_primary()))
        .child(
            div()
                .w(px(12.0))
                .flex_none()
                .text_size(ui::font_px(10.0))
                .child(if collapsed { "▸" } else { "▾" }),
        )
        .child(div().truncate().child(label.into()))
        .child(div().flex_none().child(format!("({count})")))
}

/// 弹窗自绘顶栏:标题 +(可选)副标题 + 右上角 ✕。
///
/// **为什么不用 `Dialog::title` / `close_button`**:
/// - 三个 SSH 弹窗的正文是「左栏 + 右栏」的**满幅**布局,Dialog 默认 24px 内边距
///   会把分隔线切断,所以一律 `.p_0()`,标题也就得自己画;
/// - `Dialog::close_button` 画的是 `IconName::Close`,而 0.5.1 不带 svg 资产
///   → 渲染成空白(见 `activity_bar` 模块注释),照原版画一个 `✕` 文本。
///
/// `closable = false`(保存中)时 ✕ 置灰且点不动 —— 与原版 `disabled` 同。
pub(crate) fn panel_header(
    kind_id: &'static str,
    title: impl Into<SharedString>,
    subtitle: Option<String>,
    closable: bool,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .px(px(20.0))
        .py(px(14.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.0))
                .child(
                    div()
                        .flex_1()
                        .truncate()
                        .text_size(ui::font_px(15.0))
                        .text_color(ui::text_primary())
                        .child(title.into()),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("{kind_id}-close")))
                        .flex_none()
                        .w(px(20.0))
                        .h(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(4.0))
                        .text_size(ui::font_px(12.0))
                        .text_color(ui::text_muted())
                        .opacity(if closable { 1.0 } else { 0.4 })
                        .when(closable, |el| {
                            el.cursor_pointer()
                                .hover(|el| el.text_color(ui::text_primary()).bg(ui::bg_overlay()))
                        })
                        .child("✕")
                        .on_click(move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            if closable {
                                crate::prompt::close_guarded(kind_id, window, cx);
                            }
                        }),
                ),
        )
        .when_some(subtitle, |el, subtitle| {
            el.child(
                div()
                    .truncate()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(subtitle),
            )
        })
        .into_any_element()
}

/// 弹窗底栏外壳:左边一句灰色脚注,右边的按钮由调用方 `.child()` 追加。
///
/// [`crate::ssh_assoc`] / [`crate::remote_project`] / [`crate::env_vars`] 三个
/// 弹窗的底栏容器与脚注一字不差;**按钮不并进来** —— 它们的 id、置灰口径
/// (busy / 空列表 / 校验未过)与点击语义三家各不相同,硬凑只会把三份分支塞进
/// 一个签名里。
pub(crate) fn panel_footer(hint: impl Into<SharedString>) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(20.0))
        .py(px(10.0))
        .border_t_1()
        .border_color(ui::border_subtle())
        .child(
            div()
                .flex_1()
                .text_size(ui::font_px(10.0))
                .text_color(ui::text_muted())
                .child(hint.into()),
        )
}

/// 一条连接的卡片外壳(名称 + `user@host:port` 副行)。三个弹窗共用同一款卡,
/// 差别只在左侧的勾选框/单选钮与右侧的操作按钮,由调用方追加。
pub(crate) fn conn_card(
    id: impl Into<gpui::ElementId>,
    highlight: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(12.0))
        .py(px(10.0))
        .rounded(px(6.0))
        .bg(ui::bg_base())
        .border_1()
        .border_color(if highlight {
            ui::accent()
        } else {
            ui::border_subtle()
        })
        .when(!highlight, |el| {
            el.hover(|el| el.border_color(ui::border_default()))
        })
}

/// 卡片里那两行字(名称 + 摘要)。`suffix` 接在摘要后面(「· 已存密码」)。
pub(crate) fn conn_text(conn: &SshConnection, suffix: &str) -> AnyElement {
    conn_text_with_name(
        name_line()
            .child(SharedString::from(conn.name.clone()))
            .into_any_element(),
        conn,
        suffix,
    )
}

/// 名称那一行的字号/颜色/截断 —— [`conn_text`] 与本面板的可复制名称同一款,
/// 抽出来是为了「点得动的名字」与「纯文本名字」看上去一模一样。
fn name_line() -> gpui::Div {
    div()
        .truncate()
        .text_size(ui::font_px(13.0))
        .text_color(ui::text_primary())
}

/// 名称行由调用方给的版本。本面板要把名字做成「点一下就复制」
/// (见 [`copyable_name`]),另两个弹窗不能这么干 —— 它们**整行**点击另有语义
/// (勾选 / 单选),名字上再接一个点击就成了「点哪儿结果不一样」。
fn conn_text_with_name(name: AnyElement, conn: &SshConnection, suffix: &str) -> AnyElement {
    div()
        .flex_1()
        .min_w(px(0.0))
        .flex()
        .flex_col()
        .child(name)
        .child(
            div()
                .truncate()
                .font_family("monospace")
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(SharedString::from(format!(
                    "{}{suffix}",
                    connection_summary(conn)
                ))),
        )
        .into_any_element()
}

/// 右栏折叠态住在各自面板结构的 `collapsed` 字段上 —— [`render_conn_buckets`]
/// 要改的就是这一个字段,三家的其余状态互不相干,所以只抽这一口。
pub(crate) trait BucketCollapse: 'static {
    fn collapsed_set(&mut self) -> &mut HashSet<String>;
}

/// 右栏「分组折叠 + 逐条渲染」的骨架。三个弹窗此前逐字重复三份,差别只有两处:
/// 元素 id 前缀与**行内容**(本面板的行/行内表单二选一、[`crate::ssh_assoc`]
/// 的勾选框、[`crate::remote_project`] 的单选圆点),后者由 `row` 闭包注入。
///
/// - **只有「全部」视图画桶标题**:选中某个具名分组时右栏就是那一桶,再画一遍
///   组名是废话;同理折叠只在「全部」视图下生效(`active == All` 才看 `collapsed`);
/// - `has_named` = 现在有没有具名分组。全是未分组连接时连「未分组」这个标题都
///   不画 —— 原版 `bucket.group || hasNamedGroup` 那条;
/// - 返回**一桶一个** `AnyElement`,调用方 `.children(...)` 铺进列表容器 ——
///   三家的容器 id / padding / 空态提示各不相同,那一层不并。
pub(crate) fn render_conn_buckets<T: BucketCollapse>(
    state: &Entity<T>,
    buckets: Vec<SshGroupBucket>,
    active: &GroupKey,
    collapsed: &HashSet<String>,
    has_named: bool,
    id_prefix: &'static str,
    ungrouped_label: &'static str,
    mut row: impl FnMut(&SshConnection) -> AnyElement,
) -> Vec<AnyElement> {
    let mut sections = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        let key = bucket_key(&bucket);
        let is_collapsed = *active == GroupKey::All && collapsed.contains(&key);
        let mut section = div().flex().flex_col().gap(px(6.0));
        if *active == GroupKey::All && (bucket.group.is_some() || has_named) {
            let label: SharedString = match &bucket.group {
                Some(g) => g.clone().into(),
                None => ungrouped_label.into(),
            };
            section = section.child(
                bucket_header(
                    SharedString::from(format!("{id_prefix}{key}")),
                    label,
                    bucket.items.len(),
                    is_collapsed,
                )
                .on_click({
                    let state = state.clone();
                    let key = key.clone();
                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                        let key = key.clone();
                        state.update(cx, |panel, cx| {
                            if !panel.collapsed_set().remove(&key) {
                                panel.collapsed_set().insert(key);
                            }
                            cx.notify();
                        });
                    }
                }),
            );
        }
        if !is_collapsed {
            for conn in &bucket.items {
                section = section.child(row(conn));
            }
        }
        sections.push(section.into_any_element());
    }
    sections
}

// ─── 表单纯逻辑 ───────────────────────────────────────────────

/// 端口解析。原版:`parseInt` 出来的值不是有限数 / ≤0 / >65535 一律回落 22。
pub(crate) fn parse_port(raw: &str) -> u16 {
    match raw.trim().parse::<u32>() {
        Ok(p) if p > 0 && p <= 65535 => p as u16,
        _ => 22,
    }
}

/// 「保存」按钮亮不亮(原版 `canSave = !!(name && host && user)`,全都 trim 后判)。
pub(crate) fn form_valid(name: &str, host: &str, user: &str) -> bool {
    !name.trim().is_empty() && !host.trim().is_empty() && !user.trim().is_empty()
}

/// 空串归一成 `None`(原版 `value.trim() || undefined` 与 `normalizeGroup`)。
fn opt(value: &str) -> Option<String> {
    let v = value.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// 表单值 → 一条连接。`id` 为空串时由调用方补新 id。
///
/// ⚠️ **密码不 trim**(原版 `password ? password : undefined`):前后空格是合法
/// 口令字符,替用户裁掉会让人对着「密码明明没填错」发呆。
pub(crate) fn build_connection(
    id: String,
    name: &str,
    host: &str,
    port_raw: &str,
    user: &str,
    password: &str,
    identity: &str,
    group: &str,
) -> SshConnection {
    SshConnection {
        id,
        name: name.trim().to_string(),
        host: host.trim().to_string(),
        port: parse_port(port_raw),
        user: user.trim().to_string(),
        password: if password.is_empty() {
            None
        } else {
            Some(password.to_string())
        },
        identity_file: opt(identity),
        group: opt(group),
    }
}

// ─── 拖拽载荷 ─────────────────────────────────────────────────

/// 把一条连接拖到左栏某个分组上。载荷只带连接 id —— 目标分组由落点决定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragSshConn(pub String);

/// 拖影:一张只有连接名的小卡(与 [`crate::dnd::preview`] 同款配色)。
struct ConnDragPreview(String);

impl Render for ConnDragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .px(px(10.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .bg(ui::bg_elevated())
            .border_1()
            .border_color(ui::accent())
            .text_size(ui::font_px(12.0))
            .text_color(ui::text_primary())
            .child(SharedString::from(self.0.clone()))
    }
}

// ─── 面板状态 ─────────────────────────────────────────────────

/// 编辑中的连接表单。`id` 为空串 = 新增(与原版 `emptyConnection()` 同)。
struct ConnForm {
    id: String,
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    identity: Entity<InputState>,
    group: Entity<InputState>,
}

pub struct SshPanel {
    store: Entity<AppStore>,
    selected: GroupKey,
    collapsed: HashSet<String>,
    form: Option<ConnForm>,
    /// 分组改名中:`(旧组名, 输入框)`。
    renaming: Option<(String, Entity<InputState>)>,
    /// 新建分组的输入框。
    creating: Option<Entity<InputState>>,
    /// 正被拖的连接 id(源卡据此变淡,「未分组」落点行据此恒显)。
    dragging: Option<String>,
    /// 鼠标正悬在哪个落点上(`None` = 未分组桶那一行)。
    drag_over: Option<GroupKey>,
    /// 刚复制过名字的那条连接 id —— 该行亮一枚「已复制」回执。`None` = 不亮。
    copied: Option<String>,
    /// 回执自撤任务的句柄。存着是为了「连着复制两条」时上一个计时器被丢弃 ——
    /// 否则第一条的计时器到点会把第二条刚亮起的回执提前抹掉(照 `pane.rs` 那颗气泡)。
    _copied_timer: Option<gpui::Task<()>>,
    /// 改名 / 新建两个输入框的「回车提交 / 失焦提交」订阅。
    _subs: Vec<Subscription>,
}

impl Render for SshPanel {
    /// 状态盒子。画面由 Dialog 的 builder 每帧重建(见 `modal.rs` 的说明)。
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl BucketCollapse for SshPanel {
    fn collapsed_set(&mut self) -> &mut HashSet<String> {
        &mut self.collapsed
    }
}

impl SshPanel {
    /// 提交分组改名(回车 / 失焦)。空名或没变都只是退出编辑态。
    fn commit_rename(&mut self, cx: &mut Context<Self>) {
        let Some((old, input)) = self.renaming.take() else {
            return;
        };
        self._subs.clear();
        let next = input.read(cx).value().trim().to_string();
        if !next.is_empty() && next != old {
            self.store
                .update(cx, |store, cx| store.rename_ssh_group(&old, &next, cx));
            if self.selected == GroupKey::Named(old.clone()) {
                self.selected = GroupKey::Named(next.clone());
            }
            if self.collapsed.remove(&old) {
                self.collapsed.insert(next);
            }
        }
        cx.notify();
    }

    /// 提交新建分组(回车 / 失焦)。重名时只切选中态 —— 原版同款。
    fn commit_create(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.creating.take() else {
            return;
        };
        self._subs.clear();
        let name = input.read(cx).value().trim().to_string();
        if !name.is_empty() {
            self.store
                .update(cx, |store, cx| store.create_ssh_group(&name, cx));
            self.selected = GroupKey::Named(name);
        }
        cx.notify();
    }

    fn cancel_edits(&mut self, cx: &mut Context<Self>) {
        // 先把订阅连同编辑态一起丢掉,随之而来的失焦才不会变成提交
        self._subs.clear();
        self.renaming = None;
        self.creating = None;
        cx.notify();
    }
}

/// 打开「SSH 连接」面板。
pub fn open(window: &mut Window, cx: &mut App) {
    // 守卫要在**建任何输入框之前**判(与 `prompt::show_prompt` 同一条)
    if crate::overlay::contains(crate::overlay::key(kind::SSH_PANEL)) {
        return;
    }
    let store = AppStore::global(cx);
    let state = cx.new(|_cx| SshPanel {
        store,
        selected: GroupKey::All,
        collapsed: HashSet::new(),
        form: None,
        renaming: None,
        creating: None,
        dragging: None,
        drag_over: None,
        copied: None,
        _copied_timer: None,
        _subs: Vec::new(),
    });

    open_guarded(kind::SSH_PANEL, window, cx, move |dialog, window, cx| {
        let total = panel_total_h(window.viewport_size());
        let body = render_body(&state, total, cx);
        dialog
            // 满幅左右栏:默认 24px 内边距会把中缝分隔线切断(见 `panel_header`)
            .p_0()
            .close_button(false)
            .w(px(PANEL_W))
            // 面板里有连接表单(含密码 / 私钥路径),误点遮罩关掉会丢未保存内容;
            // Esc 仍可退(原版 `closeOnOverlay={false}`)
            .overlay_closable(false)
            .child(body)
    });
}

// ─── 动作 ─────────────────────────────────────────────────────

fn start_add(state: &Entity<SshPanel>, group: Option<String>, window: &mut Window, cx: &mut App) {
    let form = new_form(None, group.unwrap_or_default(), window, cx);
    state.update(cx, |panel, cx| {
        panel.form = Some(form);
        cx.notify();
    });
}

fn start_edit(state: &Entity<SshPanel>, conn: &SshConnection, window: &mut Window, cx: &mut App) {
    let form = new_form(Some(conn), String::new(), window, cx);
    state.update(cx, |panel, cx| {
        panel.form = Some(form);
        cx.notify();
    });
}

fn new_form(
    conn: Option<&SshConnection>,
    default_group: String,
    window: &mut Window,
    cx: &mut App,
) -> ConnForm {
    let name = text_field(
        t("sshModal", "namePlaceholder"),
        conn.map(|c| c.name.clone()).unwrap_or_default(),
        false,
        window,
        cx,
    );
    // 打开即可直接改名,不必先点一下输入框(原版表单第一个框 `autoFocus`)。
    // 表单是在**已经开着的**本面板里长出来的、不经 `open_dialog`,所以焦点没有
    // 被抢的问题;仍走 `autofocus` 是为了与其余输入弹窗同一条路(它多让一轮
    // effect,输入框此刻尚未画出也不要紧)
    autofocus(&name, window, cx);
    ConnForm {
        id: conn.map(|c| c.id.clone()).unwrap_or_default(),
        name,
        host: text_field(
            t("sshModal", "hostPlaceholder"),
            conn.map(|c| c.host.clone()).unwrap_or_default(),
            false,
            window,
            cx,
        ),
        port: text_field(
            "22",
            conn.map(|c| c.port)
                .filter(|p| *p != 0)
                .unwrap_or(22)
                .to_string(),
            false,
            window,
            cx,
        ),
        user: text_field(
            t("sshModal", "userPlaceholder"),
            conn.map(|c| c.user.clone()).unwrap_or_default(),
            false,
            window,
            cx,
        ),
        // 原版是 `type="password"`;gpui-component 的对应物是 `masked`
        password: text_field(
            "",
            conn.and_then(|c| c.password.clone()).unwrap_or_default(),
            true,
            window,
            cx,
        ),
        identity: text_field(
            t("sshModal", "identityPlaceholder"),
            conn.and_then(|c| c.identity_file.clone()).unwrap_or_default(),
            false,
            window,
            cx,
        ),
        group: text_field(
            t("sshModal", "groupPlaceholder"),
            conn.and_then(|c| c.group.clone()).unwrap_or(default_group),
            false,
            window,
            cx,
        ),
    }
}

/// 一个带占位串与初值的单行输入框。**独立函数而不是闭包** —— 闭包会把
/// `&mut Window` 一直借着,同一条语句里再 `state.focus(window, cx)` 就借冲突了。
fn text_field(
    placeholder: impl Into<SharedString>,
    value: String,
    masked: bool,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .masked(masked)
            .placeholder(placeholder.into())
            .default_value(value)
    })
}

fn save_form(state: &Entity<SshPanel>, cx: &mut App) {
    let Some(conn) = state.read(cx).form.as_ref().map(|f| {
        build_connection(
            f.id.clone(),
            &f.name.read(cx).value().to_string(),
            &f.host.read(cx).value().to_string(),
            &f.port.read(cx).value().to_string(),
            &f.user.read(cx).value().to_string(),
            &f.password.read(cx).value().to_string(),
            &f.identity.read(cx).value().to_string(),
            &f.group.read(cx).value().to_string(),
        )
    }) else {
        return;
    };
    if !form_valid(&conn.name, &conn.host, &conn.user) {
        return;
    }
    let mut conn = conn;
    if conn.id.is_empty() {
        conn.id = crate::tree::gen_id("ssh");
    }
    state.update(cx, |panel, cx| {
        panel.form = None;
        panel
            .store
            .update(cx, |store, cx| store.upsert_ssh_connection(conn, cx));
        cx.notify();
    });
}

/// 删除一条连接。**不可撤销**(密码/私钥路径一并丢失)且会静默收窄已关联项目的
/// agent 可见范围,故走二次确认 —— 确认框是另一种覆盖物,叠在本面板之上。
fn delete_conn(state: &Entity<SshPanel>, conn: &SshConnection, window: &mut Window, cx: &mut App) {
    let state = state.clone();
    let id = conn.id.clone();
    Confirm::new(
        t("sshModal", "deleteConfirmTitle"),
        tr!(
            "sshModal",
            "deleteConfirmMessage",
            name = conn.name.clone(),
            summary = connection_summary(conn)
        ),
    )
    .open(
        move |_window, cx| {
            let id = id.clone();
            state.update(cx, |panel, cx| {
                panel
                    .store
                    .update(cx, |store, cx| store.remove_ssh_connection(&id, cx));
                cx.notify();
            });
        },
        window,
        cx,
    );
}

fn dissolve_group(state: &Entity<SshPanel>, name: &str, cx: &mut App) {
    let name = name.to_string();
    state.update(cx, |panel, cx| {
        panel
            .store
            .update(cx, |store, cx| store.dissolve_ssh_group(&name, cx));
        if panel.selected == GroupKey::Named(name.clone()) {
            panel.selected = GroupKey::All;
        }
        cx.notify();
    });
}

fn start_rename_group(state: &Entity<SshPanel>, name: &str, window: &mut Window, cx: &mut App) {
    let input = cx.new(|cx| InputState::new(window, cx).default_value(name.to_string()));
    autofocus(&input, window, cx);
    let name = name.to_string();
    state.update(cx, |panel, cx| {
        // 回车 = 提交,失焦 = 提交(原版 onKeyDown Enter / onBlur 两条都提交)
        let sub = cx.subscribe(&input, |this: &mut SshPanel, _i, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_rename(cx);
            }
        });
        panel._subs = vec![sub];
        panel.creating = None;
        panel.renaming = Some((name, input));
        cx.notify();
    });
}

fn start_create_group(state: &Entity<SshPanel>, window: &mut Window, cx: &mut App) {
    let input = cx
        .new(|cx| InputState::new(window, cx).placeholder(t("sshModal", "addGroupPlaceholder")));
    autofocus(&input, window, cx);
    state.update(cx, |panel, cx| {
        let sub = cx.subscribe(&input, |this: &mut SshPanel, _i, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                this.commit_create(cx);
            }
        });
        panel._subs = vec![sub];
        panel.renaming = None;
        panel.creating = Some(input);
        cx.notify();
    });
}

/// 「已复制」回执亮多久。与终端那颗选区气泡同一档(`pane.rs` 的 1s),
/// 长到看得见、短到不挡下一次操作。
const COPIED_TIP: std::time::Duration = std::time::Duration::from_secs(1);

/// 点击连接名要送进剪贴板的那一串:**连接名原文**。
///
/// 刻意不是 `user@host:port`(那一串就画在名字底下,想要它的人直接选文本更快),
/// 也不 trim —— 名字是 [`build_connection`] 存进去时就 trim 过的,这里再裁一遍
/// 只会掩盖存量配置里手改出来的怪名字。用途是把名字贴进 AI 对话 / `mini-term-ssh`
/// 那类**按名字引用连接**的地方,一个字都不能差。
pub(crate) fn copy_payload(conn: &SshConnection) -> String {
    conn.name.clone()
}

/// 复制连接名 + 亮一秒回执。
///
/// 回执画在行内而不是弹 toast:本面板是模态,toast 会落在遮罩之下 / 盖住列表,
/// 而「点了哪一行」的反馈本来就该长在那一行上。
fn copy_name(state: &Entity<SshPanel>, conn: &SshConnection, cx: &mut App) {
    cx.write_to_clipboard(gpui::ClipboardItem::new_string(copy_payload(conn)));
    let id = conn.id.clone();
    state.update(cx, |panel, cx| {
        panel.copied = Some(id);
        cx.notify();
        panel._copied_timer = Some(cx.spawn(async move |panel, cx| {
            cx.background_executor().timer(COPIED_TIP).await;
            let _ = panel.update(cx, |panel: &mut SshPanel, cx| {
                panel.copied = None;
                cx.notify();
            });
        }));
    });
}

fn move_to_group(state: &Entity<SshPanel>, conn_id: &str, group: Option<&str>, cx: &mut App) {
    let (conn_id, group) = (conn_id.to_string(), group.map(str::to_string));
    state.update(cx, |panel, cx| {
        panel.store.update(cx, |store, cx| {
            store.move_ssh_connection_to_group(&conn_id, group.as_deref(), cx)
        });
        panel.dragging = None;
        panel.drag_over = None;
        cx.notify();
    });
}

// ─── 渲染 ─────────────────────────────────────────────────────

/// 一帧要用到的只读快照。先整块读出来再画 —— 后面每个 `render_*` 都要
/// `&mut App`(建输入框 / 弹菜单),`state.read(cx)` 的借用会一路活到语句末。
struct Frame {
    connections: Vec<SshConnection>,
    named: Vec<(String, Vec<SshConnection>)>,
    ungrouped: Vec<SshConnection>,
    order: Vec<SshGroupBucket>,
    active: GroupKey,
    collapsed: HashSet<String>,
    editing_id: Option<String>,
    adding: bool,
    renaming: Option<String>,
    creating: bool,
    dragging: Option<String>,
    drag_over: Option<GroupKey>,
    copied: Option<String>,
}

fn read_frame(state: &Entity<SshPanel>, cx: &App) -> Frame {
    let panel = state.read(cx);
    let store = panel.store.read(cx);
    let connections = store.ssh_connections().to_vec();
    let buckets = build_group_buckets(&connections, store.ssh_groups());
    let group_names = buckets.group_names();
    let order = buckets.display_order();
    // 拖拽期间「未分组」那一行恒显 —— 不然连接一旦全在组里就没法拖出来
    let ungrouped_visible = !buckets.ungrouped.is_empty() || panel.dragging.is_some();
    let active = resolve_active(&panel.selected, &group_names, ungrouped_visible);
    Frame {
        connections,
        named: buckets.named.clone(),
        ungrouped: buckets.ungrouped.clone(),
        order,
        active,
        collapsed: panel.collapsed.clone(),
        editing_id: panel
            .form
            .as_ref()
            .map(|f| f.id.clone())
            .filter(|id| !id.is_empty()),
        adding: panel.form.as_ref().is_some_and(|f| f.id.is_empty()),
        renaming: panel.renaming.as_ref().map(|(n, _)| n.clone()),
        creating: panel.creating.is_some(),
        dragging: panel.dragging.clone(),
        drag_over: panel.drag_over.clone(),
        copied: panel.copied.clone(),
    }
}

fn render_body(state: &Entity<SshPanel>, total: gpui::Pixels, cx: &mut App) -> AnyElement {
    // 拖拽结束 / 中断(松手在窗外、落在非落点上)后 gpui 会清 active_drag 并重画:
    // 借这一帧把残留的 view state 清掉,否则「未分组」那一行会一直钉在栏里。
    // **不 notify** —— 正在渲染,再触发一次重画就是死循环(与 project_list 同一条)
    if !cx.has_active_drag() {
        state.update(cx, |panel, _cx| {
            panel.dragging = None;
            panel.drag_over = None;
        });
    }
    let frame = read_frame(state, cx);
    div()
        .h(total)
        .flex()
        .flex_col()
        .child(panel_header(
            kind::SSH_PANEL,
            t("sshModal", "title"),
            None,
            true,
        ))
        .child(
            div()
                .flex_1()
                .flex()
                .min_h(px(0.0))
                .child(render_sidebar(state, &frame, cx))
                .child(render_list(state, &frame, cx)),
        )
        .into_any_element()
}

fn render_sidebar(state: &Entity<SshPanel>, frame: &Frame, cx: &mut App) -> AnyElement {
    let mut bar = div()
        .id("ssh-sidebar")
        .w(px(SIDEBAR_W))
        .flex_none()
        .h_full()
        .overflow_y_scroll()
        .py(px(8.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
        .border_r_1()
        .border_color(ui::border_subtle())
        // 左栏空白右键 = 新增分组
        .on_mouse_down(MouseButton::Right, {
            let state = state.clone();
            move |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                cx.stop_propagation();
                let state = state.clone();
                menu::show(
                    event.position,
                    vec![menu::item(t("sshModal", "addGroup"), move |window, cx| {
                        start_create_group(&state, window, cx);
                    })],
                    window,
                    cx,
                );
            }
        })
        .child(
            sidebar_row(
                "ssh-group-all",
                t("sshModal", "allConnections"),
                frame.connections.len(),
                frame.active == GroupKey::All,
                false,
            )
            .on_click({
                let state = state.clone();
                move |_: &ClickEvent, _window, cx: &mut App| {
                    state.update(cx, |panel, cx| {
                        panel.selected = GroupKey::All;
                        cx.notify();
                    });
                }
            }),
        );

    for (name, items) in &frame.named {
        if frame.renaming.as_deref() == Some(name.as_str()) {
            let input = state.read(cx).renaming.as_ref().map(|(_, i)| i.clone());
            bar = bar.child(
                div()
                    .mx(px(8.0))
                    .px(px(4.0))
                    .py(px(2.0))
                    .on_key_down({
                        let state = state.clone();
                        move |event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut App| {
                            if event.keystroke.key == "escape" {
                                cx.stop_propagation();
                                state.update(cx, |panel, cx| panel.cancel_edits(cx));
                            }
                        }
                    })
                    .children(input.map(|i| Input::new(&i))),
            );
            continue;
        }
        let key = GroupKey::Named(name.clone());
        bar = bar.child(
            group_drop_target(
                sidebar_row(
                    SharedString::from(format!("ssh-group-{name}")),
                    name.clone(),
                    items.len(),
                    frame.active == key,
                    frame.drag_over.as_ref() == Some(&key),
                ),
                state,
                Some(name.clone()),
                key.clone(),
            )
            .on_click({
                let state = state.clone();
                let key = key.clone();
                move |_: &ClickEvent, _window, cx: &mut App| {
                    let key = key.clone();
                    state.update(cx, |panel, cx| {
                        panel.selected = key;
                        cx.notify();
                    });
                }
            })
            .on_mouse_down(MouseButton::Right, {
                let state = state.clone();
                let name = name.clone();
                move |event: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                    cx.stop_propagation();
                    let entries = vec![
                        menu::item(t("sshModal", "renameGroup"), {
                            let state = state.clone();
                            let name = name.clone();
                            move |window, cx| start_rename_group(&state, &name, window, cx)
                        }),
                        MenuItem::new(t("sshModal", "dissolveGroup"))
                            .danger()
                            .on_click({
                                let state = state.clone();
                                let name = name.clone();
                                move |_window, cx| dissolve_group(&state, &name, cx)
                            })
                            .into(),
                    ];
                    menu::show(event.position, entries, window, cx);
                }
            }),
        );
    }

    if !frame.ungrouped.is_empty() || frame.dragging.is_some() {
        bar = bar.child(
            group_drop_target(
                sidebar_row(
                    "ssh-group-ungrouped",
                    t("sshModal", "ungrouped"),
                    frame.ungrouped.len(),
                    frame.active == GroupKey::Ungrouped,
                    frame.drag_over.as_ref() == Some(&GroupKey::Ungrouped),
                ),
                state,
                None,
                GroupKey::Ungrouped,
            )
            .on_click({
                let state = state.clone();
                move |_: &ClickEvent, _window, cx: &mut App| {
                    state.update(cx, |panel, cx| {
                        panel.selected = GroupKey::Ungrouped;
                        cx.notify();
                    });
                }
            }),
        );
    }

    if frame.creating {
        let input = state.read(cx).creating.clone();
        bar = bar.child(
            div()
                .mx(px(8.0))
                .px(px(4.0))
                .py(px(2.0))
                .on_key_down({
                    let state = state.clone();
                    move |event: &gpui::KeyDownEvent, _window: &mut Window, cx: &mut App| {
                        if event.keystroke.key == "escape" {
                            cx.stop_propagation();
                            state.update(cx, |panel, cx| panel.cancel_edits(cx));
                        }
                    }
                })
                .children(input.map(|i| Input::new(&i))),
        );
    }

    bar.into_any_element()
}

/// 给左栏一行挂上「接收连接拖拽」的两件事:悬停高亮 + 松手改归属。
///
/// ⚠️ `on_drag_move` 会打给**每一个**注册者(见 [`crate::dnd`] 模块注释第 2 条),
/// 命中判定必须自己做 —— 漏了整栏会一起亮。
fn group_drop_target(
    el: gpui::Stateful<gpui::Div>,
    state: &Entity<SshPanel>,
    group: Option<String>,
    key: GroupKey,
) -> gpui::Stateful<gpui::Div> {
    el.on_drag_move({
        let state = state.clone();
        let key = key.clone();
        move |event: &gpui::DragMoveEvent<DragSshConn>, _window: &mut Window, cx: &mut App| {
            let hit = crate::dnd::hit_ratio(event.bounds, event.event.position).is_some();
            let key = key.clone();
            state.update(cx, |panel, cx| {
                let was = panel.drag_over.clone();
                if hit {
                    panel.drag_over = Some(key);
                } else if was.as_ref() == Some(&key) {
                    panel.drag_over = None;
                }
                if panel.drag_over != was {
                    cx.notify();
                }
            });
        }
    })
    .on_drop({
        let state = state.clone();
        move |item: &DragSshConn, _window: &mut Window, cx: &mut App| {
            move_to_group(&state, &item.0, group.as_deref(), cx);
        }
    })
}

fn render_list(state: &Entity<SshPanel>, frame: &Frame, cx: &mut App) -> AnyElement {
    let has_named = !frame.named.is_empty();
    let mut list = div()
        .id("ssh-conn-list")
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .overflow_y_scroll()
        .px(px(20.0))
        .py(px(16.0))
        .flex()
        .flex_col()
        .gap(px(12.0));

    if frame.connections.is_empty() && !frame.adding {
        list = list.child(
            div()
                .py(px(40.0))
                .flex()
                .justify_center()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(t("sshModal", "empty")),
        );
    }

    // 骨架与另两个弹窗共用(见 [`render_conn_buckets`]);本面板的行有两态 ——
    // 正在编辑的那一条原地换成表单
    list = list.children(render_conn_buckets(
        state,
        visible_buckets(&frame.order, &frame.active),
        &frame.active,
        &frame.collapsed,
        has_named,
        "ssh-bucket-",
        t("sshModal", "ungrouped"),
        |conn| {
            if frame.editing_id.as_deref() == Some(conn.id.as_str()) {
                render_form(state, cx)
            } else {
                render_row(state, conn, frame)
            }
        },
    ));

    if frame.adding {
        list = list.child(render_form(state, cx));
    } else {
        let group_for_new = match &frame.active {
            GroupKey::Named(name) => Some(name.clone()),
            _ => None,
        };
        list = list.child(
            div()
                .id("ssh-add-conn")
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .py(px(10.0))
                .rounded(px(6.0))
                .border_1()
                .border_dashed()
                .border_color(ui::border_default())
                .cursor_pointer()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_muted())
                .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
                .child(t("sshModal", "addConnection"))
                .on_click({
                    let state = state.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        start_add(&state, group_for_new.clone(), window, cx);
                    }
                }),
        );
    }

    list.child(
        div()
            .pt(px(4.0))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .text_size(ui::font_px(11.0))
            .text_color(ui::text_muted())
            .child(t("sshModal", "footerHint"))
            .child(t("sshModal", "groupOpsHint"))
            .child(t("sshModal", "copyNameHint")),
    )
    .into_any_element()
}

fn render_row(state: &Entity<SshPanel>, conn: &SshConnection, frame: &Frame) -> AnyElement {
    let id = conn.id.clone();
    let suffix = if conn.password.is_some() {
        t("sshModal", "passwordSaved").to_string()
    } else {
        String::new()
    };
    let is_source = frame.dragging.as_deref() == Some(id.as_str());
    let just_copied = frame.copied.as_deref() == Some(id.as_str());
    let conn_for_edit = conn.clone();
    let conn_for_del = conn.clone();
    let drag_label = conn.name.clone();

    conn_card(SharedString::from(format!("ssh-row-{id}")), false)
        .when(is_source, |el| el.opacity(0.4))
        .cursor_pointer()
        .on_drag(DragSshConn(id.clone()), {
            let state = state.clone();
            move |item: &DragSshConn, _offset, _window: &mut Window, cx: &mut App| {
                let dragged = item.0.clone();
                state.update(cx, |panel, _cx| panel.dragging = Some(dragged));
                cx.new(|_| ConnDragPreview(drag_label.clone()))
            }
        })
        .child(conn_text_with_name(
            copyable_name(state, conn, just_copied),
            conn,
            &suffix,
        ))
        .child(
            div()
                .flex()
                .flex_none()
                .gap(px(4.0))
                .child(
                    ui::ghost_button(
                        SharedString::from(format!("ssh-edit-{id}")),
                        t("sshModal", "edit"),
                    )
                    .on_click({
                        let state = state.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            start_edit(&state, &conn_for_edit, window, cx);
                        }
                    }),
                )
                .child(
                    ui::danger_button(
                        SharedString::from(format!("ssh-del-{id}")),
                        t("sshModal", "delete"),
                    )
                    .on_click({
                        let state = state.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            delete_conn(&state, &conn_for_del, window, cx);
                        }
                    }),
                ),
        )
        .into_any_element()
}

/// 连接名做成「点一下复制名字」的按钮 + 复制后那一秒的行内回执。
///
/// # 三处细节
///
/// 1. **不是整卡可点** —— 卡片本身是拖拽源(拖进左栏分组),整卡再接点击会让
///    「想拖却手抖点了一下」变成一次莫名其妙的复制;点击面缩到名字那几个字上,
///    与拖拽的手势区分得开(拖拽要位移,点击不要);
/// 2. **名字外面套一层 flex 而不是直接给名字挂 hover** —— 回执是名字**右边**
///    的一枚小签,名字自己得留着 `min_w(0) + truncate`,长名字才会在回执出现时
///    缩短而不是把卡片撑破;
/// 3. `cursor_pointer` 卡片上本来就有(拖拽),这里靠 hover 变 accent 色告诉用户
///    「这几个字与旁边不一样」。
fn copyable_name(state: &Entity<SshPanel>, conn: &SshConnection, just_copied: bool) -> AnyElement {
    let id = conn.id.clone();
    let conn = conn.clone();
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .min_w(px(0.0))
        .child(
            name_line()
                .id(SharedString::from(format!("ssh-name-{id}")))
                .min_w(px(0.0))
                .cursor_pointer()
                .when(just_copied, |el| el.text_color(ui::accent()))
                .hover(|el| el.text_color(ui::accent()))
                .child(SharedString::from(conn.name.clone()))
                .on_click({
                    let state = state.clone();
                    move |_: &ClickEvent, _window: &mut Window, cx: &mut App| {
                        copy_name(&state, &conn, cx);
                    }
                }),
        )
        .when(just_copied, |el| {
            el.child(
                div().flex_none().child(
                    mt_ui::CopiedTip::new(t("sshModal", "copied"))
                        .colors(ui::bg_overlay(), ui::text_primary())
                        .font_size(ui::font_px(10.0)),
                ),
            )
        })
        .into_any_element()
}

/// 新增 / 编辑表单(原版 `SshConnectionForm`:accent 虚线框里一叠带标签的字段)。
fn render_form(state: &Entity<SshPanel>, cx: &mut App) -> AnyElement {
    let Some((name, host, port, user, password, identity, group)) =
        state.read(cx).form.as_ref().map(|f| {
            (
                f.name.clone(),
                f.host.clone(),
                f.port.clone(),
                f.user.clone(),
                f.password.clone(),
                f.identity.clone(),
                f.group.clone(),
            )
        })
    else {
        return div().into_any_element();
    };
    let can_save = form_valid(
        &name.read(cx).value().to_string(),
        &host.read(cx).value().to_string(),
        &user.read(cx).value().to_string(),
    );
    // 分组下拉的候选:已有的具名分组
    let options: Vec<String> = {
        let panel = state.read(cx);
        let store = panel.store.read(cx);
        build_group_buckets(store.ssh_connections(), store.ssh_groups()).group_names()
    };

    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .p(px(12.0))
        .rounded(px(6.0))
        .bg(ui::bg_base())
        .border_1()
        .border_dashed()
        .border_color(ui::accent())
        .child(field(t("sshModal", "nameLabel"), None, Input::new(&name)))
        .child(
            div()
                .flex()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .child(field(t("sshModal", "hostLabel"), None, Input::new(&host))),
                )
                .child(
                    div()
                        .w(px(96.0))
                        .flex_none()
                        .child(field(t("sshModal", "portLabel"), None, Input::new(&port))),
                ),
        )
        .child(field(t("sshModal", "userLabel"), None, Input::new(&user)))
        .child(field(
            t("sshModal", "passwordLabel"),
            Some(t("sshModal", "passwordHint")),
            Input::new(&password),
        ))
        .child(field(
            t("sshModal", "identityLabel"),
            Some(t("sshModal", "identityHint")),
            div()
                .flex()
                .gap(px(8.0))
                .child(div().flex_1().child(Input::new(&identity)))
                .child(
                    ui::ghost_button("ssh-browse-key", "...").on_click({
                        let identity = identity.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
                                files: true,
                                directories: false,
                                multiple: false,
                                prompt: Some(t("sshModal", "selectKeyFile").into()),
                            });
                            let identity = identity.clone();
                            window
                                .spawn(cx, async move |cx| {
                                    let Ok(Ok(Some(paths))) = paths.await else {
                                        return;
                                    };
                                    let Some(path) = paths.into_iter().next() else {
                                        return;
                                    };
                                    let text = path.to_string_lossy().to_string();
                                    let _ = cx.update(|window, cx| {
                                        identity.update(cx, |s, cx| s.set_value(text, window, cx));
                                    });
                                })
                                .detach();
                        }
                    }),
                ),
        ))
        .child(field(
            t("sshModal", "groupLabel"),
            Some(t("sshModal", "groupHint")),
            div()
                .flex()
                .gap(px(8.0))
                .child(div().flex_1().child(Input::new(&group)))
                .child(ui::ghost_button("ssh-group-pick", "▾").on_click({
                    let group = group.clone();
                    move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                        let entries: Vec<menu::MenuEntry> = options
                            .iter()
                            .map(|name| {
                                let group = group.clone();
                                let name = name.clone();
                                menu::item(name.clone(), move |window, cx| {
                                    group.update(cx, |s, cx| {
                                        s.set_value(name.clone(), window, cx)
                                    });
                                })
                            })
                            .collect();
                        if entries.is_empty() {
                            return;
                        }
                        menu::show(event.position(), entries, window, cx);
                    }
                })),
        ))
        .child(
            div()
                .flex()
                .justify_end()
                .gap(px(8.0))
                .pt(px(2.0))
                .child(
                    ui::ghost_button("ssh-form-cancel", t("sshModal", "cancel")).on_click({
                        let state = state.clone();
                        move |_: &ClickEvent, _window, cx: &mut App| {
                            state.update(cx, |panel, cx| {
                                panel.form = None;
                                cx.notify();
                            });
                        }
                    }),
                )
                .child(
                    ui::primary_button("ssh-form-save", t("sshModal", "save"))
                        .opacity(if can_save { 1.0 } else { 0.4 })
                        .on_click({
                            let state = state.clone();
                            move |_: &ClickEvent, _window, cx: &mut App| save_form(&state, cx)
                        }),
                ),
        )
        .into_any_element()
}

/// 一行带标签(+ 可选灰字提示)的表单字段(原版 `Field`)。
fn field(
    label: impl Into<SharedString>,
    hint: Option<&'static str>,
    control: impl IntoElement,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(label.into()),
        )
        .child(control)
        .when_some(hint, |el, hint| {
            el.child(
                div()
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child(hint),
            )
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(group: Option<&str>) -> SshGroupBucket {
        SshGroupBucket {
            group: group.map(str::to_string),
            items: Vec::new(),
        }
    }

    /// 选中的组没了就回落「全部」;还在就保持。
    #[test]
    fn 选中的组消失后回落全部() {
        let names = vec!["内网".to_string()];
        assert_eq!(
            resolve_active(&GroupKey::Named("内网".into()), &names, false),
            GroupKey::Named("内网".into())
        );
        assert_eq!(
            resolve_active(&GroupKey::Named("没了".into()), &names, false),
            GroupKey::All
        );
        assert_eq!(resolve_active(&GroupKey::All, &names, true), GroupKey::All);
    }

    /// 「未分组」桶空且没在拖 → 那一行不画,选中态也跟着回落。
    #[test]
    fn 未分组不可见时选中态回落() {
        assert_eq!(
            resolve_active(&GroupKey::Ungrouped, &[], true),
            GroupKey::Ungrouped
        );
        assert_eq!(resolve_active(&GroupKey::Ungrouped, &[], false), GroupKey::All);
    }

    /// 「全部」展示所有桶;选中某组只剩那一个。
    #[test]
    fn 可见桶按选中态过滤() {
        let order = vec![bucket(Some("a")), bucket(Some("b")), bucket(None)];
        assert_eq!(visible_buckets(&order, &GroupKey::All).len(), 3);
        let only = visible_buckets(&order, &GroupKey::Named("b".into()));
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].group.as_deref(), Some("b"));
        let un = visible_buckets(&order, &GroupKey::Ungrouped);
        assert_eq!(un.len(), 1);
        assert!(un[0].group.is_none());
    }

    /// 端口解析:非法一律回落 22(原版 `Number.isFinite && >0 && <=65535`)。
    #[test]
    fn 端口非法回落默认值() {
        assert_eq!(parse_port("2222"), 2222);
        assert_eq!(parse_port(" 22 "), 22);
        assert_eq!(parse_port(""), 22);
        assert_eq!(parse_port("abc"), 22);
        assert_eq!(parse_port("0"), 22);
        assert_eq!(parse_port("-1"), 22);
        assert_eq!(parse_port("65536"), 22);
        assert_eq!(parse_port("65535"), 65535);
    }

    /// 三个必填项全都 trim 后判空。
    #[test]
    fn 表单必填三项() {
        assert!(form_valid("n", "h", "u"));
        assert!(!form_valid(" ", "h", "u"));
        assert!(!form_valid("n", "", "u"));
        assert!(!form_valid("n", "h", "   "));
    }

    /// 建连接:名字/主机/用户/私钥/分组 trim,空串归 `None`;**密码不 trim**。
    #[test]
    fn 建连接时空串归none且密码不裁剪() {
        let c = build_connection(
            "id1".into(),
            "  prod ",
            " 10.0.0.5 ",
            "2222",
            " root ",
            " pa ss ",
            "  ",
            "  内网 ",
        );
        assert_eq!(c.name, "prod");
        assert_eq!(c.host, "10.0.0.5");
        assert_eq!(c.port, 2222);
        assert_eq!(c.user, "root");
        assert_eq!(c.password.as_deref(), Some(" pa ss "), "密码前后空格是合法口令字符");
        assert_eq!(c.identity_file, None);
        assert_eq!(c.group.as_deref(), Some("内网"));

        let empty = build_connection("i".into(), "n", "h", "x", "u", "", "", "");
        assert_eq!(empty.password, None);
        assert_eq!(empty.group, None);
        assert_eq!(empty.port, 22);
    }

    /// 点名字复制出去的是**连接名原文**,不是底下那行 `user@host:port` 摘要,
    /// 也不做任何裁剪 —— 它要拿去当「按名字引用连接」的字面量,差一个字就对不上。
    #[test]
    fn 复制载荷是连接名原文() {
        let conn = SshConnection {
            id: "c1".into(),
            name: "生产 服务器".into(),
            host: "10.0.0.5".into(),
            port: 2222,
            user: "root".into(),
            password: None,
            identity_file: None,
            group: None,
        };
        assert_eq!(copy_payload(&conn), "生产 服务器");
        assert_ne!(copy_payload(&conn), connection_summary(&conn));
    }

    /// 折叠键:未分组桶用空串。
    #[test]
    fn 折叠键未分组用空串() {
        assert_eq!(bucket_key(&bucket(Some("g"))), "g");
        assert_eq!(bucket_key(&bucket(None)), "");
    }
}
