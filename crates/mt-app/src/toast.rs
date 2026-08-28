//! 自建 toast 层。对应 `src/components/ToastContainer.tsx`(80 行)+
//! `styles.css:537-615` + `store.ts` 里那套 TTL / 悬停暂停 / 去重的定时器。
//!
//! # 为什么不用 `gpui_component::notification::Notification`
//!
//! 读过 0.5.1 的 `notification.rs` 全文,**四条缺口是结构性的**(组件库里改不了,
//! 宿主也绕不过去),外加两条与 M/P/S 批同源的老问题:
//!
//! | 缺口 | 组件库现状 |
//! |---|---|
//! | **悬停暂停** | 没有。`NotificationList` 的 `on_hover` 只置 `expanded` 字段,而那个字段在 `render` 里根本没被读 —— 是个死字段 |
//! | **最多 5 条** | 写死 `take(10)`;`push_notification` 不返回句柄,宿主数不到也改不了 |
//! | **× 常驻** | `invisible()` + `group_hover` 才显形,图标还是 `IconName::Close` |
//! | **去重语义** | `id1::<T>(key)` 是**替换**,原版是「同项目已有就忽略」—— 方向相反 |
//! | 图标 | 四个 `NotificationType` 图标全走 `IconName` → SVG 资产,本仓没注册 `AssetSource`,渲染出来是空白且编译期无感 |
//! | 位置 / 尺寸 | 右**上**角、448px 宽;原版是右下角 16/16、280px |
//!
//! 自建代价可控:原版 toast 一共 80 行 TSX + 78 行 CSS,而且**没有任何图标资产**
//! —— 圆形徽标里就是 `✓` / `!` / `i` 三个文本字符(`ToastContainer.tsx:53`)。
//!
//! `Root::render_notification_layer` **保留不动**(组件库内部别处可能还用它),
//! 只是 mt-app 不再 `push_notification`。
//!
//! # 分工
//!
//! - [`ToastQueue`]:**纯数据**(不碰 gpui)—— 入队/去重/出队/按项目清理/取前 5 条。
//!   语义全在这里,于是全部可单测。
//! - [`ToastLayer`]:gpui 实体 —— 队列 + 每条一个 [`Task`] 定时器 + 渲染。
//!   状态住在**全局**(与 [`crate::menu`] 同一种分工):AI 泵、pane、store 三处
//!   都要往里推,而画出来的位置只有 `Workspace` 一处。
//!
//! # 生命周期(逐条照抄 `store.ts:592-609, 1235-1250`)
//!
//! - 入队即起 5s 定时器 —— **包括排在第 5 条之后还没露面的那些**(原版同样在
//!   `pushNotification` 里无条件 `armNotificationTimer`);
//! - 悬停 → 丢弃定时器句柄(暂停);移开 → **重新计满 5s**,不是续剩余;
//! - 已经消失的 toast 移开鼠标不会把它复活(`pause` 先查在不在队里)。

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use gpui::{
    AnimationExt as _, App, AppContext, Context, Entity, Global, InteractiveElement, IntoElement,
    ParentElement, Render, SharedString, StatefulInteractiveElement, Styled, Task, Window, div, px,
};

use mt_ui::tooltip::Tooltip;

use crate::i18n::t;
use crate::notify::ToastKind;
use crate::store::AppStore;
use crate::ui;

/// 自动消失时长(`store.ts:592` 的 `NOTIFICATION_TTL_MS`)。
const TTL: Duration = Duration::from_millis(5000);
/// 最多同时**渲染**几条;超出的排队等补位,**不丢**(`ToastContainer.tsx:12`)。
const MAX_VISIBLE: usize = 5;
/// 卡片宽度(`styles.css:551`)。
const CARD_WIDTH: f32 = 280.0;
/// 进场动画时长(`toastSlideIn 0.25s ease-out`)。
const SLIDE_IN_MS: u64 = 250;

/// `wsl-info` 那条用的占位项目 id(`App.tsx:369`)。**不参与任何跳转**,
/// 也不会匹配到真实项目 —— 关项目时的清理因此天然放过它。
pub const WSL_INFO_PROJECT: &str = "__wsl_info__";

/// 队列里的一条。字段与 `types.ts:306-319` 的 `AiCompletionNotification` 对齐
/// (`timestamp` 没搬:原版留着它也只是排序用,而这里本来就是插入序)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToastItem {
    pub id: u64,
    pub project_id: String,
    pub project_name: String,
    pub kind: ToastKind,
    /// 自定义正文;`None` = 按 kind 取固定文案(见 [`ToastKind::uses_message`])。
    pub message: Option<String>,
}

// ─── 纯队列(可测) ──────────────────────────────────────────

/// toast 队列的全部语义,不依赖 gpui。
#[derive(Default)]
pub struct ToastQueue {
    items: VecDeque<ToastItem>,
    next_id: u64,
}

impl ToastQueue {
    /// 入队。返回新条目的 id;`None` = 被去重挡下(什么都没发生)。
    ///
    /// **去重方向与 gpui-component 相反**:原版是「同项目已有同类未消失的 toast
    /// 就不推」(`store.ts:1033-1036`、`:1080-1082`),不是替换。判据必须连 kind
    /// 一起看 —— 只按 projectId 判的话,待确认 toast 会把随后的完成 toast 吞掉
    /// (原版注释专门写了这条)。
    ///
    /// `dedupe = false` 用于信息/错误类:原版那三条走的是裸 `pushNotification`,
    /// 一次都不去重(同一个 pane 连着两次粘贴失败该响两声)。
    pub fn push(&mut self, item: ToastItem, dedupe: bool) -> Option<u64> {
        if dedupe && self.has_live(&item.project_id, item.kind) {
            return None;
        }
        self.next_id += 1;
        let id = self.next_id;
        self.items.push_back(ToastItem { id, ..item });
        Some(id)
    }

    /// 同项目同 kind 还有没有没消失的。
    pub fn has_live(&self, project_id: &str, kind: ToastKind) -> bool {
        self.items
            .iter()
            .any(|n| n.project_id == project_id && n.kind == kind)
    }

    /// 同项目、同类且正文完全相同的提示是否仍在队列中。后台 reconcile 可能
    /// 连续命中同一阻止条件，用这条精确判定去重，不影响其它粘贴/上传错误。
    fn has_live_message(&self, project_id: &str, kind: ToastKind, message: &str) -> bool {
        self.items.iter().any(|item| {
            item.project_id == project_id
                && item.kind == kind
                && item.message.as_deref() == Some(message)
        })
    }

    /// 移除一条。返回是否真的移掉了(定时器到点时队列里可能已经没它了)。
    pub fn dismiss(&mut self, id: u64) -> bool {
        let before = self.items.len();
        self.items.retain(|n| n.id != id);
        self.items.len() != before
    }

    /// 这条还在队里吗 —— 悬停移开要**先查再重新计时**,否则会把已经消失的
    /// toast 挂上一个永远等不到主人的定时器(`store.ts:1249` 的同一道判定)。
    pub fn contains(&self, id: u64) -> bool {
        self.items.iter().any(|n| n.id == id)
    }

    /// 关项目时把它的 toast 一并清掉(`store.ts:859`)。返回被清掉的 id。
    pub fn retain_other_projects(&mut self, project_id: &str) -> Vec<u64> {
        let dropped: Vec<u64> = self
            .items
            .iter()
            .filter(|n| n.project_id == project_id)
            .map(|n| n.id)
            .collect();
        self.items.retain(|n| n.project_id != project_id);
        dropped
    }

    /// 当前该画出来的那几条(前 [`MAX_VISIBLE`] 条,先进先出)。
    pub fn visible(&self) -> impl Iterator<Item = &ToastItem> {
        self.items.iter().take(MAX_VISIBLE)
    }

    /// 队列总长(**含**没画出来的那些)。只有单测在看 —— 渲染只关心前 5 条,
    /// 而「超出 5 条不丢」正是要靠它才验得出来。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
    }
}

// ─── gpui 实体 ───────────────────────────────────────────────

#[derive(Default)]
pub struct ToastLayer {
    queue: ToastQueue,
    /// 每条一个自撤定时器。悬停暂停 = **丢句柄**(Task 一 drop 就取消),
    /// 移开 = 重建一个满 5s 的。
    timers: HashMap<u64, Task<()>>,
}

struct GlobalToastLayer(Entity<ToastLayer>);
impl Global for GlobalToastLayer {}

/// 建出 toast 层并登记为全局。**必须早于任何视图** —— 起 PTY 那一步就可能推
/// WSL 提示,而它发生在窗口打开之前。
pub fn init(cx: &mut App) {
    let entity = cx.new(|_| ToastLayer::default());
    cx.set_global(GlobalToastLayer(entity));
}

/// toast 层实体。宿主(`Workspace`)拿它当子视图画出来。
pub fn layer(cx: &App) -> Entity<ToastLayer> {
    cx.global::<GlobalToastLayer>().0.clone()
}

/// 推一条 AI 提醒 toast(完成 / 待确认)。同项目同类还没消失就**忽略**。
pub fn push_alert(kind: ToastKind, project_id: String, project_name: String, cx: &mut App) {
    push_item(
        ToastItem {
            id: 0,
            project_id,
            project_name,
            kind,
            message: None,
        },
        true,
        cx,
    );
}

/// 推一条自带正文的 toast(信息提示 / 移动端会话 / 粘贴失败)。**不去重**。
pub fn push_message(
    kind: ToastKind,
    project_id: String,
    project_name: String,
    message: String,
    cx: &mut App,
) {
    push_item(
        ToastItem {
            id: 0,
            project_id,
            project_name,
            kind,
            message: Some(message),
        },
        false,
        cx,
    );
}

/// 推一条幂等的自定义提示。只压掉仍存活的“同项目 + 同 kind + 同正文”，用于
/// 可重复触发的后台 reconcile；不同错误正文仍可并存。
pub fn push_message_deduped(
    kind: ToastKind,
    project_id: String,
    project_name: String,
    message: String,
    cx: &mut App,
) {
    let item = ToastItem {
        id: 0,
        project_id,
        project_name,
        kind,
        message: Some(message),
    };
    layer(cx).update(cx, |layer, cx| {
        let message = item.message.as_deref().unwrap_or_default();
        if layer
            .queue
            .has_live_message(&item.project_id, item.kind, message)
        {
            return;
        }
        let Some(id) = layer.queue.push(item, false) else {
            return;
        };
        layer.arm(id, cx);
        cx.notify();
    });
}

/// WSL 启动器重写的一次性告知(`App.tsx:367-379`)。
///
/// 项目名是**合成**的 `WSL: {distro}`、项目 id 是占位串 —— 原版就是这么推的,
/// 它不属于任何项目,点一下只关闭。「一次性」的语义是每个新 PTY 各推一次,
/// 不去重(同款)。
pub fn push_wsl_override(distro: &str, unix_path: &str, cx: &mut App) {
    push_message(
        ToastKind::WslInfo,
        WSL_INFO_PROJECT.to_string(),
        format!("WSL: {distro}"),
        crate::i18n::tr!("app", "wslOverride", path = unix_path),
        cx,
    );
}

/// 关项目 → 它的 toast 一并撤掉(`store.ts:859`)。
pub fn remove_project(project_id: &str, cx: &mut App) {
    layer(cx).update(cx, |layer, cx| {
        for id in layer.queue.retain_other_projects(project_id) {
            layer.timers.remove(&id);
        }
        cx.notify();
    });
}

fn push_item(item: ToastItem, dedupe: bool, cx: &mut App) {
    layer(cx).update(cx, |layer, cx| {
        let Some(id) = layer.queue.push(item, dedupe) else {
            return;
        };
        layer.arm(id, cx);
        cx.notify();
    });
}

impl ToastLayer {
    /// 给某条挂一个满 5s 的自撤定时器(已有的那个随句柄被覆盖而取消)。
    fn arm(&mut self, id: u64, cx: &mut Context<Self>) {
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(TTL).await;
            let _ = this.update(cx, |this: &mut Self, cx| this.dismiss(id, cx));
        });
        self.timers.insert(id, task);
    }

    fn dismiss(&mut self, id: u64, cx: &mut Context<Self>) {
        self.timers.remove(&id);
        if self.queue.dismiss(id) {
            cx.notify();
        }
    }

    /// 悬停暂停。5s 硬倒计时会在鼠标正要点它的时候把它抽走。
    /// 移开时**重新计满 5s**(原版就是重新 arm,不是续剩余)。
    fn pause(&mut self, id: u64, paused: bool, cx: &mut Context<Self>) {
        if paused {
            self.timers.remove(&id);
        } else if self.queue.contains(id) {
            self.arm(id, cx);
        }
    }

    /// 点卡片:该跳的跳,一律关掉自己。
    fn activate(&mut self, id: u64, window: &mut Window, cx: &mut Context<Self>) {
        let target = self
            .queue
            .visible()
            .find(|n| n.id == id)
            .map(|n| (n.kind, n.project_id.clone()));
        self.dismiss(id, cx);

        let Some(project_id) = target
            .filter(|(kind, _)| kind.jumps_to_project())
            .map(|(_, project_id)| project_id)
        else {
            return;
        };

        // ⚠️ **必须 defer**:切项目会 `hydrate_project` → 起 PTY,而起 PTY 有一条
        // 支路会推 WSL 提示 toast(见 `TerminalPane::new`)。此刻我们正身处
        // `ToastLayer` 自己的 update 里,那一推就是同一实体的嵌套 update ——
        // gpui 当场 panic。`window.defer` 把整段挪到本轮 effect 之后,
        // 那时 ToastLayer 的借用早已释放。
        window.defer(cx, move |window, cx| {
            let store = AppStore::global(cx);
            // 队列是异步消失的,点下去时那个项目可能已经被删了
            if store.read(cx).project(&project_id).is_none() {
                return;
            }
            // 原版只 `setActiveProject`;GPUI 侧一并跳到那个项目的待办 pane
            // (`main.rs` 旧 `deliver_alert` 已经是这个行为,不退回去)
            let pane = store.read(cx).next_attention_target(Some(&project_id));
            let jumps_to_pane = pane.is_some();
            store.update(cx, |store, cx| {
                store.set_active_project(&project_id, cx);
                if let Some((pid, pane_id)) = pane {
                    store.activate_pane(&pid, &pane_id, window, cx);
                }
            });
            if jumps_to_pane {
                crate::workbench_area::activate_terminal_page(window, cx);
            }
        });
    }
}

/// 圆形徽标的底色(`styles.css:576-583`)。
fn icon_color(kind: ToastKind) -> gpui::Hsla {
    match kind {
        ToastKind::Completion => ui::color_success(),
        ToastKind::Attention => ui::color_warning(),
        ToastKind::PasteError => ui::color_error(),
        ToastKind::WslInfo | ToastKind::MobileSession => ui::color_info(),
    }
}

/// 卡片左侧那条 3px 竖边的颜色。
///
/// ⚠️ 原版**只有 attention 换色**(`.toast-card--attention`),
/// `paste-error` / `wsl-info` 的左边框照样是绿的 —— 看着像疏漏,但这是可见外观,
/// 照抄不改(要改是产品决定,不是迁移决定)。
fn border_left_color(kind: ToastKind) -> gpui::Hsla {
    match kind {
        ToastKind::Attention => ui::color_warning(),
        _ => ui::color_success(),
    }
}

/// 正文。信息/错误类取 `message`,其余按 kind 取固定文案
/// (判定链照抄 `ToastContainer.tsx:57-63`)。
fn description(item: &ToastItem) -> SharedString {
    if item.kind.uses_message() {
        return item.message.clone().unwrap_or_default().into();
    }
    match item.kind {
        ToastKind::Attention => t("toast", "aiAttention").into(),
        _ => t("toast", "aiDone").into(),
    }
}

impl Render for ToastLayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 先拍快照:下面每条都要 `cx.listener`,边遍历边借 self 不划算
        let visible: Vec<ToastItem> = self.queue.visible().cloned().collect();

        // `.toast-stack`:`fixed right:16 bottom:16; flex-col; gap:8`。
        // 容器本身**不 occlude**(原版 `pointer-events:none`),卡片自己收事件。
        let mut stack = div()
            .absolute()
            .right(px(16.0))
            .bottom(px(16.0))
            .w(px(CARD_WIDTH))
            .flex()
            .flex_col()
            .items_end()
            .gap(px(8.0));

        for item in visible {
            let id = item.id;
            let kind = item.kind;
            let desc = description(&item);
            let card = div()
                .id(SharedString::from(format!("toast-{id}")))
                .relative()
                .w_full()
                .flex()
                // ⚠️ 这一层**不能** `items_center`:左侧那条 3px 竖边靠 flex 的
                // 默认 `stretch` 撑满高度,居中会把它压成 0
                .rounded(px(6.0))
                .overflow_hidden()
                .bg(ui::bg_elevated())
                .border_1()
                .border_color(ui::border_default())
                .shadow_lg()
                .cursor_pointer()
                .occlude()
                // `.toast-card:hover { transform: translateX(-2px) }`
                .hover(|el| el.left(px(-2.0)))
                .child(
                    // `border-left: 3px solid ...` 的等价物。
                    //
                    // gpui 的 `border_color` 是**四边一个色**,没有 per-side ——
                    // 写成 `border_1().border_color(A).border_l(3).border_color(B)`
                    // 会把整圈边框都染成 B。改画一条 3px 的实心竖条,外层
                    // `overflow_hidden` + `rounded(6)` 顺带把它的两个角切圆。
                    div().w(px(3.0)).flex_none().bg(border_left_color(kind)),
                )
                .child(
                    // 卡片正文:`padding: 10px 12px; gap: 10px`(原版的内边距在
                    // 边框之内,所以挪到这一层)
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .px(px(12.0))
                        .py(px(10.0))
                        .child(
                            // 20px 圆底 + 文本字符(原版就是文本,不是 svg)
                            div()
                                .w(px(20.0))
                                .h(px(20.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(icon_color(kind))
                                // `color: var(--bg-base)`;亮色主题那条 `#ffffff` 覆盖与
                                // 亮色 `--bg-base` 恰好同值,不必分支
                                .text_color(ui::bg_base())
                                .text_size(ui::font_px(11.0))
                                .font_weight(gpui::FontWeight::BOLD)
                                .child(kind.icon_char()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .flex()
                                .flex_col()
                                .child(
                                    // `.toast-name` 0.92rem / 600 / 单行省略
                                    div()
                                        .text_size(ui::font_px(12.0))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(ui::text_primary())
                                        .truncate()
                                        .child(SharedString::from(item.project_name.clone())),
                                )
                                .child(
                                    // `.toast-desc` 0.77rem,`margin-top: 1px`
                                    div()
                                        .mt(px(1.0))
                                        .text_size(ui::font_px(10.0))
                                        .text_color(ui::text_secondary())
                                        .child(desc),
                                ),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("toast-close-{id}")))
                                .px(px(4.0))
                                .flex_none()
                                .cursor_pointer()
                                .text_size(ui::font_px(14.0))
                                .text_color(ui::text_muted())
                                .hover(|el| el.text_color(ui::text_primary()))
                                // 原版这颗按钮带 `aria-label` / `title`
                                // (`ToastContainer.tsx:69-70`);GPUI 没有无障碍树,
                                // tooltip 是它唯一的等价物
                                .tooltip(move |window, cx| {
                                    Tooltip::new(t("toast", "dismiss")).build(window, cx)
                                })
                                .child("×")
                                .on_click(cx.listener(move |this, _event, _window, cx| {
                                    // 关按钮吃掉这次点击 —— 不然会连带触发卡片的「跳项目」
                                    cx.stop_propagation();
                                    this.dismiss(id, cx);
                                })),
                        ),
                )
                .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                    this.pause(id, *hovered, cx);
                }))
                .on_click(cx.listener(move |this, _event, window, cx| {
                    this.activate(id, window, cx);
                }));

            // `toastSlideIn 0.25s ease-out`:opacity 0→1 且 translateX(100%)→0。
            // gpui 没有 transform,用相对定位的 `left` 补一条等效位移
            // (容器宽度就是卡片宽度,所以 100% = CARD_WIDTH)。
            //
            // ⚠️ 过减弱动效的闸:`.toast-card` **不在**原版 reduce 的豁免名单里
            // (豁免的是浮层进出场、切终端、用量面板那几类),通配规则把它压成
            // 瞬时 —— 这里等价成「直接上终态,连动画元素都不挂」。
            stack = stack.child(if mt_ui::motion::reduce_motion() {
                card.into_any_element()
            } else {
                card.with_animation(
                    SharedString::from(format!("toast-slide-{id}")),
                    gpui::Animation::new(Duration::from_millis(SLIDE_IN_MS))
                        .with_easing(ui::cubic_bezier(0.0, 0.0, 0.58, 1.0)),
                    |el, delta| el.opacity(delta).left(px(CARD_WIDTH * (1.0 - delta))),
                )
                .into_any_element()
            });
        }
        stack
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(project: &str, kind: ToastKind) -> ToastItem {
        ToastItem {
            id: 0,
            project_id: project.into(),
            project_name: format!("项目 {project}"),
            kind,
            message: None,
        }
    }

    /// TTL 与上限是照抄来的常量,别在重构里悄悄漂了
    /// (组件库那边分别是「没有暂停」与 10 条)。
    #[test]
    fn ttl_与上限与原版一致() {
        assert_eq!(TTL, Duration::from_millis(5000));
        assert_eq!(MAX_VISIBLE, 5);
    }

    /// 入队发号、按插入序出队。
    #[test]
    fn 入队按插入序发号() {
        let mut q = ToastQueue::default();
        let a = q.push(item("p1", ToastKind::Completion), true).unwrap();
        let b = q.push(item("p2", ToastKind::Completion), true).unwrap();
        assert_ne!(a, b);
        let ids: Vec<u64> = q.visible().map(|n| n.id).collect();
        assert_eq!(ids, vec![a, b]);
    }

    /// 去重方向:同项目同 kind 已有就**忽略**(不是替换)。
    #[test]
    fn 同项目同类去重为忽略() {
        let mut q = ToastQueue::default();
        let first = q.push(item("p1", ToastKind::Completion), true).unwrap();
        assert_eq!(q.push(item("p1", ToastKind::Completion), true), None);
        assert_eq!(q.len(), 1);
        assert_eq!(q.visible().next().unwrap().id, first, "留的是先来的那条");
    }

    /// 待确认与完成**各计各的**:同一个项目两种 toast 能并存 ——
    /// 只按 projectId 判的话,待确认会把随后的完成 toast 吞掉。
    #[test]
    fn 完成与待确认互不去重() {
        let mut q = ToastQueue::default();
        assert!(q.push(item("p1", ToastKind::Completion), true).is_some());
        assert!(q.push(item("p1", ToastKind::Attention), true).is_some());
        assert_eq!(q.len(), 2);
    }

    /// 信息/错误类不去重:同一个 pane 连着两次粘贴失败该响两声。
    #[test]
    fn 信息与错误类不去重() {
        let mut q = ToastQueue::default();
        assert!(q.push(item("p1", ToastKind::PasteError), false).is_some());
        assert!(q.push(item("p1", ToastKind::PasteError), false).is_some());
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn 后台提示只按相同正文精确去重() {
        let mut q = ToastQueue::default();
        let mut blocked = item("p1", ToastKind::PasteError);
        blocked.message = Some("有未保存文档".into());
        q.push(blocked, false).unwrap();

        assert!(q.has_live_message("p1", ToastKind::PasteError, "有未保存文档"));
        assert!(!q.has_live_message("p1", ToastKind::PasteError, "上传失败"));
        assert!(!q.has_live_message("p2", ToastKind::PasteError, "有未保存文档"));
    }

    /// 超出 5 条只是**不画**,不丢 —— 前面的消失后自动补位。
    #[test]
    fn 超出五条排队补位() {
        let mut q = ToastQueue::default();
        let ids: Vec<u64> = (0..7)
            .map(|i| {
                q.push(item(&format!("p{i}"), ToastKind::Completion), true)
                    .unwrap()
            })
            .collect();
        assert_eq!(q.len(), 7);
        assert_eq!(q.visible().count(), 5);
        assert_eq!(
            q.visible().map(|n| n.id).collect::<Vec<_>>(),
            ids[..5].to_vec()
        );
        q.dismiss(ids[0]);
        assert_eq!(
            q.visible().map(|n| n.id).collect::<Vec<_>>(),
            ids[1..6].to_vec(),
            "第 6 条补位"
        );
    }

    /// 出队幂等:定时器到点时那条可能已经被点掉了。
    #[test]
    fn 重复出队不报错也不误伤() {
        let mut q = ToastQueue::default();
        let id = q.push(item("p1", ToastKind::Completion), true).unwrap();
        assert!(q.dismiss(id));
        assert!(!q.dismiss(id), "第二次没东西可删");
        assert!(!q.contains(id));
    }

    /// 悬停移开要先查在不在队里 —— 已经消失的不许被重新挂上定时器。
    #[test]
    fn 出队后不再认这条_id() {
        let mut q = ToastQueue::default();
        let id = q.push(item("p1", ToastKind::Completion), true).unwrap();
        assert!(q.contains(id));
        q.dismiss(id);
        assert!(!q.contains(id));
    }

    /// 关项目连带清 toast,别的项目一条不动。
    #[test]
    fn 关项目清掉它的_toast() {
        let mut q = ToastQueue::default();
        let a = q.push(item("p1", ToastKind::Completion), true).unwrap();
        let b = q.push(item("p2", ToastKind::Completion), true).unwrap();
        let c = q.push(item("p1", ToastKind::Attention), true).unwrap();
        let dropped = q.retain_other_projects("p1");
        assert_eq!(dropped, vec![a, c]);
        assert_eq!(q.visible().map(|n| n.id).collect::<Vec<_>>(), vec![b]);
    }

    /// WSL 提示挂的是占位项目 id,**不会**被任何真实项目的关闭清掉。
    #[test]
    fn wsl_提示不随项目关闭消失() {
        let mut q = ToastQueue::default();
        let wsl = q
            .push(item(WSL_INFO_PROJECT, ToastKind::WslInfo), false)
            .unwrap();
        q.push(item("p1", ToastKind::Completion), true).unwrap();
        q.retain_other_projects("p1");
        assert_eq!(q.visible().map(|n| n.id).collect::<Vec<_>>(), vec![wsl]);
    }

    /// 正文来源:信息/错误类取 message,AI 两类取固定文案。
    #[test]
    fn 正文按_kind_取来源() {
        let mut wsl = item(WSL_INFO_PROJECT, ToastKind::WslInfo);
        wsl.message = Some("已改用 wsl.exe".into());
        assert_eq!(description(&wsl), SharedString::from("已改用 wsl.exe"));

        let done = item("p1", ToastKind::Completion);
        assert_eq!(description(&done), SharedString::from(t("toast", "aiDone")));
        let attention = item("p1", ToastKind::Attention);
        assert_eq!(
            description(&attention),
            SharedString::from(t("toast", "aiAttention"))
        );

        // message 缺失的信息类退化成空串,而不是掉回 aiDone(那会张冠李戴)
        let empty = item(WSL_INFO_PROJECT, ToastKind::WslInfo);
        assert_eq!(description(&empty), SharedString::from(""));
    }
}
