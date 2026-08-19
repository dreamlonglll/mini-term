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
//! # 自定义元素子菜单([`MenuItem::submenu_element`])
//!
//! 原版 `MenuEntry.submenuRender(host) => cleanup` 的等价物:菜单项悬停时在旁侧
//! 挂**任意自绘面板**(会话分支家族树就是这么挂上去的)。与普通子菜单共用同一套
//! 坐标(父项右缘 `left:100% / top:-4px` + `anchored` 贴边收拢)、同一套展开互斥
//! (`open_path`)与同一套关闭语义(点外 / Esc / 悬停同层别的项),差别只在
//! 内容由调用方给一个 `Fn(&mut Window, &mut App) -> AnyElement`。
//!
//! **清理**:原版返回一个 `cleanup`(`root.unmount()`),这里靠**闭包本身的所有权**
//! —— 菜单收起时 `entries` 整份 drop,闭包捕获的东西(通常是个 `Entity`)随之释放。
//! 与原版的一处偏差:原版在子菜单**收起**时就 unmount(再悬停要重新拉数据),
//! 这里闭包活到整个菜单关闭为止,所以调用方缓存的实体在一次菜单生命周期内复用,
//! 反复悬停不重扫磁盘。
//!
//! # 焦点与键盘导航
//!
//! 打开时把焦点收到菜单上(Esc 要有人接),关闭时**先还给打开前那个元素、再执行
//! 菜单项动作**。顺序照抄原版的注释:动作可能同步打开一个输入弹窗并聚焦输入框,
//! 反过来的话还原焦点会把光标从那个输入框上抢走。
//!
//! 原版的键盘导航靠**逐项 DOM 焦点**(每个 `.ctx-menu-item` 都 `tabIndex=-1`,
//! ↑↓ 直接 `.focus()` 下一项,高亮由 `:focus-visible` 与 `:hover` 共用同一条
//! CSS 规则)。这里焦点**只有菜单容器一个**(上面那条纪律的前提:关闭时要能把
//! 焦点原样还回去,逐项收焦点会让「打开前那个元素」记不准),所以选中项改成一份
//! **视图状态** [`OpenMenu::active`](逐层下标的完整路径),高亮自己画成与 hover
//! 同样的底色 —— 观感与原版的 `:focus-visible` 一致。
//!
//! 逐键对照 `contextMenu.ts::onKey`:↑↓ 在**焦点所在那一层**里绕圈(悬停展开了
//! 子菜单但焦点还在上层时,↓ 不会跳进子菜单)、→ 在子菜单父项上展开并进第一项、
//! ← 收起最深一层并把选中还给**展开它的那一项**、Enter/Space 等价于点击、
//! Esc 关闭。原版没有的键(Home/End/首字母跳转)这里也没有。
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

/// 自绘子菜单面板的内容构造器(原版 `submenuRender`)。
///
/// 子菜单展开期间**每次菜单重绘都会调一遍**,所以里面别做重活 —— 有状态的面板
/// (要拉数据那种)在闭包里懒建一次 `Entity` 缓存起来,之后只
/// `clone().into_any_element()`。
pub type SubmenuRender = Rc<dyn Fn(&mut Window, &mut App) -> AnyElement>;

/// 菜单里的一条。与 `contextMenu.ts` 的 `MenuEntry` 一一对应。
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
    /// 不可交互的分组标题(`.ctx-menu-header`)。
    ///
    /// 唯一的真实消费方是终端右键的「SSH 连接」子菜单(BB-b 接上):
    /// 连接按 group 分段,段与段之间靠它隔开。
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
    /// 自绘子菜单面板。与 [`submenu`](Self::submenu) 互斥(两者都给时以它为准),
    /// 展开/坐标/关闭语义完全相同,只是内容由调用方画。
    submenu_element: Option<SubmenuRender>,
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
            submenu_element: None,
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

    /// 挂一块**自绘面板**当子菜单(原版 `submenuRender`)。
    ///
    /// 悬停展开、坐标、贴边、关闭全部走菜单机制,调用方只管内容。
    /// 有状态的面板(要拉数据的那种)在闭包外建 `Entity`,闭包里只 clone:
    ///
    /// ```ignore
    /// let panel = cx.new(|cx| BranchFamilyPanel::new(..., cx));
    /// MenuItem::new(label).submenu_element(move |_window, _cx| panel.clone().into_any_element())
    /// ```
    pub fn submenu_element(
        mut self,
        render: impl Fn(&mut Window, &mut App) -> AnyElement + 'static,
    ) -> Self {
        self.submenu_element = Some(Rc::new(render));
        self
    }

    /// 这一项是子菜单父项吗(普通子菜单 / 自绘面板都算)。
    fn has_submenu(&self) -> bool {
        !self.submenu.is_empty() || self.submenu_element.is_some()
    }

    /// 这一项点得动吗(禁用 / 子菜单父项 / 没挂动作都点不动)。
    /// 渲染与测试共用同一个判据,免得两边漂移。
    pub fn is_actionable(&self) -> bool {
        !self.disabled && !self.has_submenu() && self.on_click.is_some()
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
    /// 键盘选中项的**完整路径**(末位是它在自己那层里的下标)。`None` = 还没用
    /// 键盘选过 —— 与原版「activeElement 还在菜单外」是同一个状态,此时 ↓ 从
    /// 第一项开始、↑ 从最后一项开始。
    active: Option<Vec<usize>>,
    /// 打开菜单前的焦点,关闭时还回去。
    prev_focus: Option<FocusHandle>,
    focus: FocusHandle,
    /// 根面板进场(`menuPopIn`)。
    pop_in: mt_ui::motion::Transition,
    /// **最深那层**刚展开的子面板的进场。原版每建一个 `.ctx-menu` 元素就播一次,
    /// 这里只有一份状态 —— 够用,因为同一时刻只可能有一层是「刚展开的」。
    submenu_in: mt_ui::motion::Transition,
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
            active: None,
            prev_focus,
            focus,
            pop_in: mt_ui::motion::Transition::new(mt_ui::motion::MENU_IN),
            // 还没有子面板,建成「已跑完」的:第一次真展开时才 restart
            submenu_in: mt_ui::motion::Transition::settled(mt_ui::motion::MENU_IN),
        });
        cx.notify();
    });
}

/// 关掉当前菜单(幂等)。焦点还给打开菜单前的那个元素。
///
/// 基件的对称入口:菜单内部的三条关闭路径(点项 / 点外 / Esc)都走
/// [`ContextMenu::dismiss`],这个是留给**外部**主动收菜单用的。
///
/// 目前唯一的调用点是自绘子菜单面板里的可点节点([`crate::branch_family`]):
/// 面板嵌在菜单项里、被菜单面板的 `occlude` 挡着,点击冒泡不到全窗遮罩,
/// 只能自己把菜单收掉。
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

/// `ancestors` 这一层的面板现在画着吗。
///
/// 根层(`[]`)恒真;子面板要祖先链**整条**都展开。悬停别处会把子面板收掉,
/// 原版那一刻 `document.activeElement` 变成 `body`(节点没了),键盘作用域随之
/// 退回根菜单 —— 这个判定就是那件事的等价物。
fn panel_open(open_path: &[usize], ancestors: &[usize]) -> bool {
    open_path.starts_with(ancestors)
}

/// 展开路径从 `prev` 变成 `next` 时,是否有**新面板**出现(要播进场动画)。
///
/// 收起不算:`next` 是 `prev` 的前缀时留下的那些面板并没有重建,原版也不会
/// 重播动画(`closeSubmenusFrom` 只 `remove()` 深层的,浅层元素原地不动)。
fn opens_new_panel(prev: &[usize], next: &[usize]) -> bool {
    !next.is_empty() && !prev.starts_with(next)
}

/// 该层里**键盘选得中**的条目下标。
///
/// 判据照抄原版的选择器 `.ctx-menu-item:not(.disabled)`:分隔线与分组标题不是
/// 菜单项、禁用项排除;子菜单父项**算**(→ 要在它身上展开),没挂动作的普通项
/// 也算(与鼠标能不能点中是两回事,原版同)。
fn focusable_indices(entries: &[MenuEntry]) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| match e {
            MenuEntry::Item(item) if !item.disabled => Some(i),
            _ => None,
        })
        .collect()
}

/// ↑↓ 之后选中项的新下标(在 `focusables` 里绕圈)。
///
/// `current` 不在表里(没选过 / 那一项刚被收掉)时:↓ 从头、↑ 从尾 ——
/// 原版 `moveFocus` 里 `idx < 0` 那一支。
fn step_active(focusables: &[usize], current: Option<usize>, forward: bool) -> Option<usize> {
    if focusables.is_empty() {
        return None;
    }
    let pos = current.and_then(|c| focusables.iter().position(|i| *i == c));
    let next = match pos {
        None => {
            if forward {
                0
            } else {
                focusables.len() - 1
            }
        }
        Some(p) => {
            if forward {
                (p + 1) % focusables.len()
            } else {
                (p + focusables.len() - 1) % focusables.len()
            }
        }
    };
    focusables.get(next).copied()
}

/// 解析 `ancestors` 这一层的条目表。
///
/// 自绘面板([`MenuItem::submenu_element`])没有条目表,那一层返回 `None` ——
/// 键盘在它里面什么都选不中,与原版一致(自绘宿主里没有 `.ctx-menu-item`,
/// `querySelector` 落空,焦点留在父项上)。
fn entries_at<'a>(root: &'a [MenuEntry], ancestors: &[usize]) -> Option<&'a [MenuEntry]> {
    let mut level = root;
    for index in ancestors {
        match level.get(*index) {
            Some(MenuEntry::Item(item)) if !item.submenu.is_empty() => level = &item.submenu,
            _ => return None,
        }
    }
    Some(level)
}

/// 这一项是不是键盘选中的那一项(高亮判据)。
fn is_active_item(active: Option<&[usize]>, ancestors: &[usize], index: usize) -> bool {
    match active.and_then(<[usize]>::split_last) {
        Some((&last, head)) => last == index && head == ancestors,
        None => false,
    }
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

    /// 换展开路径。变了才动;有**新面板**出现时顺带重播子面板进场动画。
    /// 悬停与 →/← 都走这一条,免得两边对「什么时候算新面板」有两套看法。
    fn set_open_path(&mut self, next: Vec<usize>) -> bool {
        let Some(open) = self.open.as_mut() else {
            return false;
        };
        if open.open_path == next {
            return false;
        }
        if opens_new_panel(&open.open_path, &next) {
            open.submenu_in.restart();
        }
        open.open_path = next;
        true
    }

    /// 键盘选中项的 (所在层, 层内下标)。所在面板已经被收掉的话算没选中。
    fn active_pos(&self) -> Option<(Vec<usize>, usize)> {
        let open = self.open.as_ref()?;
        let (&index, ancestors) = open.active.as_deref()?.split_last()?;
        panel_open(&open.open_path, ancestors).then(|| (ancestors.to_vec(), index))
    }

    /// 取某一项(层 + 下标)。
    fn item_at(&self, ancestors: &[usize], index: usize) -> Option<&MenuItem> {
        let open = self.open.as_ref()?;
        match entries_at(&open.entries, ancestors)?.get(index)? {
            MenuEntry::Item(item) => Some(item),
            _ => None,
        }
    }

    /// ↑↓:在**选中项所在那一层**里绕圈。
    ///
    /// 作用域刻意不取「最后展开的那一层」——子菜单是悬停展开的,鼠标划过父项后
    /// 再按 ↓,用户看着的还是上一层。原版 `focusableItems()` 那段注释说的就是这条。
    fn move_active(&mut self, forward: bool) -> bool {
        let (scope, current) = match self.active_pos() {
            Some((ancestors, index)) => (ancestors, Some(index)),
            None => (Vec::new(), None),
        };
        let next = {
            let Some(open) = self.open.as_ref() else {
                return false;
            };
            let Some(entries) = entries_at(&open.entries, &scope) else {
                return false;
            };
            step_active(&focusable_indices(entries), current, forward)
        };
        let Some(next) = next else {
            return false;
        };
        let mut path = scope;
        path.push(next);
        if let Some(open) = self.open.as_mut() {
            open.active = Some(path);
        }
        true
    }

    /// →:在子菜单父项上展开,并落进新那层的第一项(自绘面板没有项可落,
    /// 选中留在父项上 —— 原版同)。
    fn open_active_submenu(&mut self) -> bool {
        let Some((ancestors, index)) = self.active_pos() else {
            return false;
        };
        let first = {
            let Some(item) = self.item_at(&ancestors, index) else {
                return false;
            };
            if item.disabled || !item.has_submenu() {
                return false;
            }
            focusable_indices(&item.submenu).first().copied()
        };
        let mut path = ancestors;
        path.push(index);
        self.set_open_path(path.clone());
        if let Some(first) = first {
            path.push(first);
            if let Some(open) = self.open.as_mut() {
                open.active = Some(path);
            }
        }
        true
    }

    /// ←:只收起**最深一层**,选中还给展开它的那一项(一个菜单里可能有好几个
    /// 子菜单入口,不能笼统还给「第一个父项」)。
    fn close_deepest_submenu(&mut self) -> bool {
        let Some(open) = self.open.as_ref() else {
            return false;
        };
        if open.open_path.is_empty() {
            return false;
        }
        let owner = open.open_path.clone();
        let mut next = owner.clone();
        next.pop();
        self.set_open_path(next);
        if let Some(open) = self.open.as_mut() {
            open.active = Some(owner);
        }
        true
    }

    /// 一次按键。返回**这一下有没有被菜单吃掉**(吃掉了就 `stop_propagation`,
    /// 对应原版的 `preventDefault + stopPropagation`)。
    fn on_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match key {
            "escape" => {
                self.dismiss(window, cx);
                true
            }
            "down" | "up" => {
                if self.move_active(key == "down") {
                    cx.notify();
                }
                // 方向键无论有没有挪动都算菜单消费掉了(原版同:整个 case 都
                // preventDefault),否则会漏给底下的列表去滚
                true
            }
            "right" => {
                if self.open_active_submenu() {
                    cx.notify();
                    return true;
                }
                false
            }
            "left" => {
                if self.close_deepest_submenu() {
                    cx.notify();
                    return true;
                }
                false
            }
            "enter" | "space" => {
                let Some((ancestors, index)) = self.active_pos() else {
                    return false;
                };
                let handler = self
                    .item_at(&ancestors, index)
                    .filter(|item| item.is_actionable())
                    .and_then(|item| item.on_click.clone());
                // 选中的是子菜单父项 / 禁用项时不做事,但这一下仍算菜单吃掉了
                // (原版:active 只要是 `.ctx-menu-item` 就 preventDefault)
                if let Some(handler) = handler {
                    // 与鼠标点击同一条顺序:先收菜单(顺带还焦点)再跑动作
                    self.dismiss(window, cx);
                    handler(window, cx);
                }
                true
            }
            _ => false,
        }
    }

    /// 根面板。单独一层是为了让 `self` 的不可变借用不跨到 `render` 的其余部分。
    ///
    /// `window` 一路透传到项级 —— 自绘子菜单面板([`MenuItem::submenu_element`])
    /// 的构造器要它。
    fn render_root_panel(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.open.as_ref() {
            Some(open) => self.render_panel(&open.entries, Vec::new(), window, cx),
            None => div().into_any_element(),
        }
    }

    fn render_panel(
        &self,
        entries: &[MenuEntry],
        ancestors: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open_path = self
            .open
            .as_ref()
            .map(|o| o.open_path.clone())
            .unwrap_or_default();
        let active = self.open.as_ref().and_then(|o| o.active.clone());
        // 刚展开的那一层播进场(`menuPopIn`)。判据是「这一层就是最深的展开层」:
        // 收起时留下的浅层面板并没有重建,不该跟着重播。根面板走 `pop_in`,
        // 不在这里。
        let pop_in = (!ancestors.is_empty() && ancestors == open_path)
            .then(|| self.open.as_ref().map(|o| o.submenu_in.drive(window)))
            .flatten()
            .map(mt_ui::motion::menu_pop_in);

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
            .when_some(pop_in, |el, (opacity, dy)| {
                el.opacity(opacity).mt(px(dy))
            })
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
                    panel = panel.child(self.render_item(
                        item,
                        &ancestors,
                        index,
                        &open_path,
                        active.as_deref(),
                        window,
                        cx,
                    ));
                }
            }
        }

        panel.into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_item(
        &self,
        item: &MenuItem,
        ancestors: &[usize],
        index: usize,
        open_path: &[usize],
        active: Option<&[usize]>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_submenu = item.has_submenu();
        let disabled = item.disabled;
        let danger = item.danger;
        let handler = item.on_click.clone();
        // 键盘选中项:原版靠 `:focus-visible`,而它与 `:hover` 共用同一条样式规则
        // (`styles.css:653-657`),所以这里画的底色与 hover 完全一致
        let key_active = !disabled && is_active_item(active, ancestors, index);

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
            .when(key_active, |el| {
                if danger {
                    el.bg(with_alpha(ui::color_error(), 0.15))
                } else {
                    el.bg(with_alpha(ui::accent(), 0.2))
                        .text_color(ui::text_primary())
                }
            })
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                // 只认「进入」:子菜单是悬停展开的,移开并不收起(与原版一致 ——
                // 收起发生在悬停同层的别的项、或整个菜单关闭时)
                if !*hovered {
                    return;
                }
                let next = next_open_path(&hover_ancestors, index, has_submenu);
                if this.set_open_path(next) {
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
            // 自绘面板优先:两者都给时以它为准(见 `submenu_element` 的字段注释)
            let submenu = match item.submenu_element.as_ref() {
                Some(render) => render(window, cx),
                None => self.render_panel(&item.submenu, child_path, window, cx),
            };
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
        // 根面板进场(`.ctx-menu { animation: menuPopIn }`)。**浮层豁免面**:
        // 系统开着「减少动画」照播(见 `mt_ui::motion` 的豁免表)。
        let (pop_opacity, pop_dy) = mt_ui::motion::menu_pop_in(
            self.open
                .as_ref()
                .map(|open| open.pop_in.drive(window))
                .unwrap_or(1.0),
        );
        let panel = self.render_root_panel(window, cx);
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
                                        .opacity(pop_opacity)
                                        .mt(px(pop_dy))
                                        .on_key_down(cx.listener(
                                            |this, event: &KeyDownEvent, window, cx| {
                                                if this.on_key(
                                                    event.keystroke.key.as_str(),
                                                    window,
                                                    cx,
                                                ) {
                                                    cx.stop_propagation();
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

    /// 自绘子菜单(`submenu_element`)与普通子菜单在**父项语义**上完全等价:
    /// 都算子菜单父项(画 `▸`、悬停展开),自身都点不动。
    #[test]
    fn 自绘子菜单与普通子菜单父项语义一致() {
        let plain = MenuItem::new("a").submenu(vec![separator()]);
        let custom = MenuItem::new("a").submenu_element(|_, _| div().into_any_element());
        assert!(plain.has_submenu());
        assert!(custom.has_submenu(), "自绘面板也算子菜单父项");
        assert!(!MenuItem::new("a").has_submenu());

        // 挂了动作也点不动 —— 悬停展开的项点自己不该触发动作
        let custom = MenuItem::new("a")
            .on_click(|_, _| {})
            .submenu_element(|_, _| div().into_any_element());
        assert!(!custom.is_actionable());

        // 展开路径的计算对两种父项同一套(`has_submenu` 是唯一入参)
        assert_eq!(next_open_path(&[], 2, custom.has_submenu()), vec![2]);
    }

    /// 键盘能选中的是「非禁用的菜单项」——分隔线、分组标题跳过,
    /// 子菜单父项算(→ 要落在它身上展开)。
    #[test]
    fn 可选项跳过分隔线与禁用项() {
        let entries = vec![
            MenuEntry::Header("组".into()),
            item("a", |_, _| {}),
            separator(),
            MenuItem::new("b").disabled(true).into(),
            MenuItem::new("c").submenu(vec![item("c1", |_, _| {})]).into(),
        ];
        assert_eq!(focusable_indices(&entries), vec![1, 4]);
        assert!(focusable_indices(&[]).is_empty());
        // 一项都选不中的菜单(全是分隔线)不该让 ↑↓ 选出东西来
        assert_eq!(step_active(&[], None, true), None);
    }

    /// ↑↓ 在同层里绕圈;没选中时 ↓ 从头、↑ 从尾(原版 `idx < 0` 那一支)。
    #[test]
    fn 方向键在同层里绕圈() {
        let f = vec![1, 4, 5];
        assert_eq!(step_active(&f, None, true), Some(1));
        assert_eq!(step_active(&f, None, false), Some(5));
        assert_eq!(step_active(&f, Some(1), true), Some(4));
        assert_eq!(step_active(&f, Some(5), true), Some(1), "末项 ↓ 绕回首项");
        assert_eq!(step_active(&f, Some(1), false), Some(5), "首项 ↑ 绕到末项");
        // 选中项已经不在可选表里(那一层被收掉了)→ 按「没选中」处理
        assert_eq!(step_active(&f, Some(9), true), Some(1));
        assert_eq!(step_active(&f, Some(9), false), Some(5));
    }

    /// 面板是否画着:根层恒真,子面板要祖先链整条展开。
    #[test]
    fn 层是否开着() {
        assert!(panel_open(&[], &[]), "根面板永远开着");
        assert!(panel_open(&[2], &[]));
        assert!(panel_open(&[2], &[2]));
        assert!(panel_open(&[2, 1], &[2]));
        assert!(!panel_open(&[2], &[3]), "开的是别人那一层");
        assert!(!panel_open(&[], &[2]), "子面板已收起");
    }

    /// 只有**新面板**出现才重播进场:收起(新路径是旧路径的前缀)时留下的
    /// 面板并没有重建,不能跟着闪一下。
    #[test]
    fn 新面板出现才播进场() {
        assert!(opens_new_panel(&[], &[2]), "根上展开一层");
        assert!(opens_new_panel(&[2], &[2, 1]), "再深一层");
        assert!(opens_new_panel(&[2], &[3]), "换一个父项 = 换了个新面板");
        assert!(!opens_new_panel(&[2, 1], &[2]), "收起最深一层不算");
        assert!(!opens_new_panel(&[2], &[]), "全收起不算");
        assert!(!opens_new_panel(&[2], &[2]), "没变不算");
    }

    /// 选中高亮只认「路径完全相同」的那一项。
    #[test]
    fn 选中高亮逐项判定() {
        assert!(is_active_item(Some(&[2]), &[], 2));
        assert!(!is_active_item(Some(&[2]), &[], 1));
        assert!(!is_active_item(Some(&[2]), &[2], 0), "父项不算选中子项");
        assert!(is_active_item(Some(&[2, 1]), &[2], 1));
        assert!(!is_active_item(Some(&[2, 1]), &[], 2), "祖先不跟着高亮");
        assert!(!is_active_item(None, &[], 0));
        assert!(!is_active_item(Some(&[]), &[], 0));
    }

    /// 逐层解析条目表;自绘面板那一层没有条目表。
    #[test]
    fn 按路径取到那一层的条目() {
        let entries = vec![
            item("a", |_, _| {}),
            MenuItem::new("sub")
                .submenu(vec![
                    item("s0", |_, _| {}),
                    MenuItem::new("deep")
                        .submenu(vec![item("d0", |_, _| {})])
                        .into(),
                ])
                .into(),
            MenuItem::new("custom")
                .submenu_element(|_, _| div().into_any_element())
                .into(),
        ];
        assert_eq!(entries_at(&entries, &[]).map(<[MenuEntry]>::len), Some(3));
        assert_eq!(entries_at(&entries, &[1]).map(<[MenuEntry]>::len), Some(2));
        assert_eq!(entries_at(&entries, &[1, 1]).map(<[MenuEntry]>::len), Some(1));
        // 自绘面板 / 普通项 / 越界下标:都没有下一层
        assert!(entries_at(&entries, &[2]).is_none(), "自绘面板没有条目表");
        assert!(entries_at(&entries, &[0]).is_none());
        assert!(entries_at(&entries, &[9]).is_none());
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
