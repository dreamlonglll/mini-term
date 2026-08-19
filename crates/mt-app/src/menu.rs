//! 通用右键菜单基件。对应 `src/utils/contextMenu.ts` + `styles.css` 的 `.ctx-menu*`。
//!
//! # 为什么不用 gpui-component 的 `ContextMenu` / `PopupMenu`
//!
//! 组件库确实带了这两件(`menu/context_menu.rs`、`menu/popup_menu.rs`),四条硬伤
//! 逐条挡在原版观感前面:
//!
//! 1. **图标要 SVG 资产**。`PopupMenu` 的勾选项画 `Icon::new(IconName::Check)`、
//!    子菜单箭头画 `IconName::ChevronRight`,而 0.5.1 不带 svg 资产、宿主也没注册
//!    `AssetSource` —— 渲染出来是**空白**且编译期无感(与 M 批边条图标同一个坑)。
//!    原版的勾选恰恰是「`✓ ` / 全角空格前缀」文本方案,箭头是 CSS `::after` 的 `▸`,
//!    照抄文本反而与原版一字不差。
//! 2. **没有 danger 态**,也没有右对齐的快捷键标签。要补只能逐项走
//!    `PopupMenuItem::element` 自绘,那时行渲染已经全是自己写的了。
//! 3. **配色取 `cx.theme()`**(组件库自己那套 token),而壳里其它每一处都取
//!    [`crate::ui`] 的 `Palette`。菜单是浮层,底色/边框/hover 与面板对不上最扎眼。
//! 4. **`ContextMenu` 是元素包装器,右键即弹**,而终端区的右键必须先问
//!    `prefers_local_handling`(应用抓鼠标时右键要上报给应用,不能弹本地菜单)。
//!    包装器没有这个闸门,只能自己挂 `on_mouse_down`。
//!
//! 于是照 `contextMenu.ts` 的语义自建:**命令式**入口 [`show`](自带「先关上一个」)、
//! 浮层定位在鼠标点、点外/Esc 关闭、hover 展开子菜单、分隔线/标题/danger/禁用/
//! 快捷键标签齐活。
//!
//! # 层级与定位
//!
//! ```text
//! deferred(priority 1)                  ← 画在所有常规内容之上
//!  └─ anchored(0,0)
//!      └─ 全窗口透明遮罩(occlude + on_mouse_down = 关闭)
//!          └─ anchored(鼠标点).snap_to_window_with_margin(4px)  ← 贴边自动收拢
//!              └─ 菜单面板(occlude —— 面板内的点击不算「点外」)
//!                  └─ 子菜单 absolute left:100%(挂在父项里,跟着父项走)
//! ```
//!
//! 贴边翻转由 `snap_to_window_with_margin` 白拿(原版是自己算 `placeInViewport`);
//! **子菜单不翻转** —— 它挂在父项右缘,窗口右边缘外的那点差别留作已知缺口。
//!
//! # 焦点
//!
//! 打开时把焦点收到菜单上(Esc 要有人接),关闭时**先还给打开前那个元素、再执行
//! 菜单项动作**。顺序照抄原版的注释:动作可能同步打开一个输入弹窗并聚焦输入框,
//! 反过来的话还原焦点会把光标从那个输入框上抢走。
//!
//! # 覆盖物栈
//!
//! 菜单开着时全局快捷键要让路(原版把 `'menu'` 也压进 `overlayStack`),所以
//! [`show`] / [`ContextMenu::dismiss`] 各自压栈/摘栈,见 [`crate::overlay`]。

use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Global, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Pixels, Point, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, anchored, deferred, div, point,
    prelude::FluentBuilder, px, relative,
};

use crate::overlay;
use crate::ui;

/// 菜单项被点中时跑的动作。
pub type MenuHandler = Rc<dyn Fn(&mut Window, &mut App)>;

/// 菜单里的一条。与 `contextMenu.ts` 的 `MenuEntry` 一一对应。
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
    /// 不可交互的分组标题(`.ctx-menu-header`)。
    ///
    /// 基件带着它是因为原版的分组树菜单(「移动到分组」)靠它分段;
    /// 那批功能还没做,所以本轮四处菜单都没用上。
    #[allow(dead_code)]
    Header(SharedString),
}

/// 一个可点的菜单项。
pub struct MenuItem {
    label: SharedString,
    /// 右侧那串弱化的快捷键提示,**只展示不参与匹配**(与原版同)。
    shortcut: Option<SharedString>,
    danger: bool,
    disabled: bool,
    /// 非空 = 该项是子菜单父项:悬停展开,自身点击不触发动作。
    submenu: Vec<MenuEntry>,
    on_click: Option<MenuHandler>,
}

impl MenuItem {
    pub fn new(label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            shortcut: None,
            danger: false,
            disabled: false,
            submenu: Vec::new(),
            on_click: None,
        }
    }

    pub fn on_click(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_click = Some(Rc::new(f));
        self
    }

    pub fn shortcut(mut self, label: impl Into<SharedString>) -> Self {
        self.shortcut = Some(label.into());
        self
    }

    /// 破坏性动作(删除/移除):红字 + 红底 hover。
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    pub fn submenu(mut self, entries: Vec<MenuEntry>) -> Self {
        self.submenu = entries;
        self
    }

    /// 这一项点得动吗(禁用 / 子菜单父项 / 没挂动作都点不动)。
    /// 渲染与测试共用同一个判据,免得两边漂移。
    pub fn is_actionable(&self) -> bool {
        !self.disabled && self.submenu.is_empty() && self.on_click.is_some()
    }
}

impl From<MenuItem> for MenuEntry {
    fn from(item: MenuItem) -> Self {
        MenuEntry::Item(item)
    }
}

/// `MenuItem::new(label).on_click(f)` 的简写 —— 菜单大半是这种项。
pub fn item(label: impl Into<SharedString>, f: impl Fn(&mut Window, &mut App) + 'static) -> MenuEntry {
    MenuEntry::Item(MenuItem::new(label).on_click(f))
}

pub fn separator() -> MenuEntry {
    MenuEntry::Separator
}

/// 快捷键提示串。照抄 `src/utils/hotkeys.ts::comboLabel`:
/// 非 mac 是 `Ctrl+Shift+D`,mac 是 `⌘⇧D`(符号直接拼、不加分隔符)。
///
/// 键位表的事实来源仍是 `main.rs` 里那串 `KeyBinding`,这里只负责**显示**;
/// 两处对不上不会有编译错误,改键位时记得一起改(与原版同一风险)。
pub fn hotkey_label(with_mod: bool, shift: bool, alt: bool, key: &str) -> String {
    let mac = cfg!(target_os = "macos");
    let mut parts: Vec<&str> = Vec::new();
    if with_mod {
        parts.push(if mac { "⌘" } else { "Ctrl" });
    }
    if shift {
        parts.push(if mac { "⇧" } else { "Shift" });
    }
    if alt {
        parts.push(if mac { "⌥" } else { "Alt" });
    }
    parts.push(key);
    parts.join(if mac { "" } else { "+" })
}

// ─── 全局状态 ─────────────────────────────────────────────────

/// 当前打开的那一个菜单。同时只可能有一个(原版靠模块级 `currentCleanup` 保证,
/// 这里靠「状态只有一份」天然保证)。
struct OpenMenu {
    /// 鼠标点(窗口坐标)。
    position: Point<Pixels>,
    entries: Vec<MenuEntry>,
    /// 展开中的子菜单路径(逐层下标)。`[]` = 没展开任何子菜单。
    open_path: Vec<usize>,
    /// 打开菜单前的焦点,关闭时还回去。
    prev_focus: Option<FocusHandle>,
    focus: FocusHandle,
}

#[derive(Default)]
pub struct ContextMenu {
    open: Option<OpenMenu>,
}

struct GlobalContextMenu(Entity<ContextMenu>);
impl Global for GlobalContextMenu {}

/// 建出菜单层并登记为全局。**必须早于任何视图** —— 视图的右键回调里要取它。
pub fn init(cx: &mut App) {
    let entity = cx.new(|_| ContextMenu::default());
    cx.set_global(GlobalContextMenu(entity));
}

/// 菜单层实体。宿主(`Workspace`)拿它当子视图画出来。
pub fn layer(cx: &App) -> Entity<ContextMenu> {
    cx.global::<GlobalContextMenu>().0.clone()
}

/// 在 `position` 处弹一个菜单。已经开着的那个先关掉(与原版 `showContextMenu`
/// 开头的 `currentCleanup()` 同语义)。
pub fn show(
    position: Point<Pixels>,
    entries: Vec<MenuEntry>,
    window: &mut Window,
    cx: &mut App,
) {
    if entries.is_empty() {
        return;
    }
    let entity = layer(cx);
    entity.update(cx, |menu, cx| {
        // 换菜单时焦点不重新记:上一个菜单已经把焦点收走了,再记一次会把
        // 「打开前那个元素」覆盖成菜单自己,关闭后焦点就回不去终端了。
        let prev_focus = match menu.open.take() {
            Some(prev) => prev.prev_focus,
            None => window.focused(cx),
        };
        // 换菜单时这一步是空操作(已经在栈里),照调不误 —— 压栈是幂等的
        overlay::push(overlay::key(overlay::kind::MENU));
        let focus = cx.focus_handle();
        window.focus(&focus);
        menu.open = Some(OpenMenu {
            position,
            entries,
            open_path: Vec::new(),
            prev_focus,
            focus,
        });
        cx.notify();
    });
}

/// 关掉当前菜单(幂等)。焦点还给打开菜单前的那个元素。
///
/// 基件的对称入口:菜单内部的三条关闭路径(点项 / 点外 / Esc)都走
/// [`ContextMenu::dismiss`],这个是留给**外部**主动收菜单用的
/// (切项目、窗口失焦之类),本轮还没有调用点。
#[allow(dead_code)]
pub fn close(window: &mut Window, cx: &mut App) {
    let entity = layer(cx);
    entity.update(cx, |menu, cx| menu.dismiss(window, cx));
}

// ─── 纯逻辑(可测) ────────────────────────────────────────────

/// 悬停 `ancestors` 这一层的第 `index` 项之后,展开路径该变成什么。
///
/// - 悬停子菜单父项 → 在本层路径后追加自己(展开它);
/// - 悬停普通项 → 截到本层(收掉本层展开的子菜单,**祖先层不动**)。
///
/// 这正是原版 `closeSubmenusFrom(level)` 的两个分支。
fn next_open_path(ancestors: &[usize], index: usize, has_submenu: bool) -> Vec<usize> {
    let mut next = ancestors.to_vec();
    if has_submenu {
        next.push(index);
    }
    next
}

/// `ancestors` 这一层的第 `index` 项的子菜单当前是否展开。
///
/// 只看该层那一格:更深的层次由子菜单自己再判一次,所以孙菜单展开时
/// 祖先链上的每一层都算「展开」。
fn is_submenu_open(open_path: &[usize], ancestors: &[usize], index: usize) -> bool {
    open_path.len() > ancestors.len()
        && open_path.starts_with(ancestors)
        && open_path[ancestors.len()] == index
}

/// 元素 id:同一帧里逐项唯一,且跨帧稳定(路径 + 下标)。
fn entry_id(ancestors: &[usize], index: usize) -> SharedString {
    let mut s = String::from("ctx");
    for a in ancestors {
        s.push('-');
        s.push_str(&a.to_string());
    }
    s.push_str("-i");
    s.push_str(&index.to_string());
    SharedString::from(s)
}

/// 乘性改 alpha(与 `ui::alpha` 同语义,那个是私有的)。
fn with_alpha(color: Hsla, a: f32) -> Hsla {
    Hsla {
        a: a.clamp(0.0, 1.0),
        ..color
    }
}

// ─── 渲染 ─────────────────────────────────────────────────────

impl ContextMenu {
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = self.open.take() else {
            return;
        };
        overlay::pop(overlay::key(overlay::kind::MENU));
        if let Some(prev) = open.prev_focus {
            window.focus(&prev);
        }
        cx.notify();
    }

    /// 根面板。单独一层是为了让 `self` 的不可变借用不跨到 `render` 的其余部分。
    fn render_root_panel(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.open.as_ref() {
            Some(open) => self.render_panel(&open.entries, Vec::new(), cx),
            None => div().into_any_element(),
        }
    }

    fn render_panel(
        &self,
        entries: &[MenuEntry],
        ancestors: Vec<usize>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open_path = self
            .open
            .as_ref()
            .map(|o| o.open_path.clone())
            .unwrap_or_default();

        let mut panel = div()
            .flex()
            .flex_col()
            .min_w(px(160.0))
            .p(px(4.0))
            .rounded(px(6.0))
            .border_1()
            .border_color(ui::border_default())
            .bg(ui::bg_overlay())
            .shadow_lg()
            // 面板内的按下不算「点外」—— 遮罩的 on_mouse_down 靠 hitbox 判定,
            // occlude 一挡,遮罩就收不到这一下了
            .occlude();

        for (index, entry) in entries.iter().enumerate() {
            match entry {
                MenuEntry::Separator => {
                    panel = panel.child(
                        div()
                            .h(px(1.0))
                            .my(px(4.0))
                            .bg(ui::border_subtle()),
                    );
                }
                MenuEntry::Header(text) => {
                    panel = panel.child(
                        div()
                            .px(px(12.0))
                            .pt(px(6.0))
                            .pb(px(3.0))
                            .text_size(ui::font_px(10.0))
                            .text_color(ui::text_muted())
                            .child(text.clone()),
                    );
                }
                MenuEntry::Item(item) => {
                    panel = panel.child(self.render_item(item, &ancestors, index, &open_path, cx));
                }
            }
        }

        panel.into_any_element()
    }

    fn render_item(
        &self,
        item: &MenuItem,
        ancestors: &[usize],
        index: usize,
        open_path: &[usize],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_submenu = !item.submenu.is_empty();
        let disabled = item.disabled;
        let danger = item.danger;
        let handler = item.on_click.clone();

        let text_color = if danger {
            ui::color_error()
        } else {
            ui::text_secondary()
        };

        let hover_ancestors = ancestors.to_vec();
        let submenu_open = has_submenu && is_submenu_open(open_path, ancestors, index);

        let mut row = div()
            .id(entry_id(ancestors, index))
            .relative()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(20.0))
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(4.0))
            .text_size(ui::font_px(12.0))
            .text_color(text_color)
            // 禁用项:原版是 `opacity: .4` + hover 不变色
            .when(disabled, |el| el.opacity(0.4))
            .when(!disabled, |el| {
                el.cursor_pointer().hover(move |el| {
                    if danger {
                        el.bg(with_alpha(ui::color_error(), 0.15))
                    } else {
                        el.bg(with_alpha(ui::accent(), 0.2))
                            .text_color(ui::text_primary())
                    }
                })
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                // 只认「进入」:子菜单是悬停展开的,移开并不收起(与原版一致 ——
                // 收起发生在悬停同层的别的项、或整个菜单关闭时)
                if !*hovered {
                    return;
                }
                let Some(open) = this.open.as_mut() else {
                    return;
                };
                let next = next_open_path(&hover_ancestors, index, has_submenu);
                if open.open_path != next {
                    open.open_path = next;
                    cx.notify();
                }
            }))
            .child(div().child(item.label.clone()));

        if let Some(shortcut) = item.shortcut.clone() {
            row = row.child(
                div()
                    .flex_none()
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .opacity(0.8)
                    .child(shortcut),
            );
        }
        if has_submenu {
            // 原版是 `.has-submenu::after { content: '▸' }`
            row = row.child(
                div()
                    .flex_none()
                    .text_size(ui::font_px(10.0))
                    .text_color(ui::text_muted())
                    .child("▸"),
            );
        }

        // 子菜单:挂在父项里、绝对定位到父项右缘(原版是 `rect.right - 2, rect.top - 4`)。
        //
        // 外面再套一层不带 position 的 `anchored`:它默认以**自己的布局位置**为锚点,
        // 于是位置照旧、还白拿了贴边收拢 —— 「项目类型」那 15 项的子菜单在靠近
        // 屏幕下缘的项目行上展开时,不至于有一半掉到窗口外面去。
        if submenu_open {
            let mut child_path = ancestors.to_vec();
            child_path.push(index);
            let submenu = self.render_panel(&item.submenu, child_path, cx);
            row = row.child(
                div()
                    .absolute()
                    .left(relative(1.0))
                    .top(px(-4.0))
                    .child(anchored().snap_to_window_with_margin(px(4.0)).child(submenu)),
            );
        }

        match (item.is_actionable(), handler) {
            (true, Some(handler)) => {
                row = row.on_click(cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    // 先收菜单(顺带把焦点还回去)再跑动作 —— 动作可能同步开一个
                    // 输入弹窗并聚焦输入框,反过来的话还原焦点会把光标从那儿抢走
                    this.dismiss(window, cx);
                    handler(window, cx);
                }));
            }
            // 子菜单父项 / 禁用项:点自己不触发动作、也不关菜单
            _ => row = row.on_click(|_event, _window, cx| cx.stop_propagation()),
        }

        row.into_any_element()
    }
}

impl Render for ContextMenu {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some((position, focus)) = self
            .open
            .as_ref()
            .map(|open| (open.position, open.focus.clone()))
        else {
            return div();
        };
        let panel = self.render_root_panel(cx);
        let size = window.viewport_size();

        div().child(
            deferred(
                anchored().position(point(px(0.0), px(0.0))).child(
                    div()
                        .w(size.width)
                        .h(size.height)
                        // 点菜单外任意处关闭(原版挂 document 的 click)。
                        // occlude 让这层吃掉点击 —— 否则关菜单那一下会同时点到
                        // 底下的终端/项目行,成了「关菜单顺便切了个项目」
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                                this.dismiss(window, cx);
                            }),
                        )
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(|this, _event: &MouseDownEvent, window, cx| {
                                this.dismiss(window, cx);
                            }),
                        )
                        .child(
                            anchored()
                                .position(position)
                                .snap_to_window_with_margin(px(4.0))
                                .child(
                                    div()
                                        .track_focus(&focus)
                                        .key_context("ContextMenu")
                                        .on_key_down(cx.listener(
                                            |this, event: &KeyDownEvent, window, cx| {
                                                if event.keystroke.key == "escape" {
                                                    cx.stop_propagation();
                                                    this.dismiss(window, cx);
                                                }
                                            },
                                        ))
                                        .child(panel),
                                ),
                        ),
                ),
            )
            .with_priority(1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 悬停子菜单父项 = 展开它;悬停普通项 = 收掉本层的子菜单。
    #[test]
    fn 悬停展开路径按层收放() {
        assert_eq!(next_open_path(&[], 3, true), vec![3]);
        assert_eq!(next_open_path(&[], 3, false), Vec::<usize>::new());
        // 二层:祖先链保留,只动本层
        assert_eq!(next_open_path(&[3], 1, true), vec![3, 1]);
        assert_eq!(next_open_path(&[3], 1, false), vec![3]);
    }

    /// 展开判定只看本层那一格,孙菜单展开时祖先链每层都算展开。
    #[test]
    fn 子菜单展开判定() {
        assert!(is_submenu_open(&[2], &[], 2));
        assert!(!is_submenu_open(&[2], &[], 1));
        assert!(!is_submenu_open(&[], &[], 0));
        // 孙菜单:根层第 2 项与它下面第 1 项都算展开
        assert!(is_submenu_open(&[2, 1], &[], 2));
        assert!(is_submenu_open(&[2, 1], &[2], 1));
        assert!(!is_submenu_open(&[2, 1], &[2], 0));
        // 路径分叉:前缀对不上就不算
        assert!(!is_submenu_open(&[2, 1], &[3], 1));
    }

    /// 元素 id 逐项唯一、跨层不撞。
    #[test]
    fn 菜单项_id_逐项唯一() {
        assert_eq!(entry_id(&[], 0).to_string(), "ctx-i0");
        assert_eq!(entry_id(&[1], 0).to_string(), "ctx-1-i0");
        assert_ne!(entry_id(&[1], 0), entry_id(&[], 1));
        assert_ne!(entry_id(&[1, 2], 0), entry_id(&[1], 2));
    }

    /// 快捷键串照 `comboLabel` 的规则拼(本机平台那一支)。
    #[test]
    fn 快捷键显示串() {
        let label = hotkey_label(true, true, false, "D");
        if cfg!(target_os = "macos") {
            assert_eq!(label, "⌘⇧D");
        } else {
            assert_eq!(label, "Ctrl+Shift+D");
            assert_eq!(hotkey_label(false, false, false, "F2"), "F2");
            assert_eq!(hotkey_label(true, false, true, "←"), "Ctrl+Alt+←");
        }
    }

    /// 「点得动」的判据:禁用 / 子菜单父项 / 没挂动作都点不动。
    #[test]
    fn 可点判定() {
        assert!(MenuItem::new("a").on_click(|_, _| {}).is_actionable());
        assert!(!MenuItem::new("a").is_actionable());
        assert!(
            !MenuItem::new("a")
                .on_click(|_, _| {})
                .disabled(true)
                .is_actionable()
        );
        assert!(
            !MenuItem::new("a")
                .on_click(|_, _| {})
                .submenu(vec![separator()])
                .is_actionable()
        );
    }
}
