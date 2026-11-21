//! 全应用快捷键的**唯一事实来源**。对应 `src/utils/hotkeys.ts`。
//!
//! 这张表同时被三处消费:
//!
//! 1. [`bind_keys`] —— 启动时往 gpui 注册默认键位(原来是 `main.rs` 里一串裸
//!    `KeyBinding::new`,没有分组也没有描述 key);用户改过的那几条由
//!    [`apply_overrides`] 压上去(启动时一次,设置页每改一次一次);
//! 2. 设置 →「快捷键」页 —— 按 [`groups`] 分组渲染,可改的行能录制新键位;
//! 3. 菜单 / tooltip / 首启引导里的键位标注 —— 一律按 id 取 [`hotkey_label`],拿到的是
//!    **此刻真正绑着的**键位(用户改过就是改过的那个,解绑了就是空)。
//!
//! 原版把「绑定」与「说明」分开写过一阵,结论写在 `SettingsModal.tsx:1659-1663` 的注释里:
//! 「改了键位忘改说明就直接漂移」。所以这里同样收成一张表,而且**显示串不另存**:
//! 一律从真正绑上去的键位串推导([`keystroke_label_for`]),两者没有机会对不上 ——
//! [`tests::显示串从实际绑定推导`] 拿进了 keymap 的那条 binding 反推一遍钉住。
//!
//! # 键位选择原则(照抄 `hotkeys.ts` 的开头注释)
//!
//! 终端应用的第一约束是**不能吞掉 shell / TUI 需要的按键**。裸 `Ctrl+T`
//! (bash transpose-chars)、`Ctrl+W`(删除前一个词)、`Ctrl+P`(上一条历史)都有
//! 既定语义,因此应用级动作统一走 `Ctrl+Shift+*`(Windows Terminal / VS Code
//! 终端的惯例),只有确实不与行编辑冲突的才用裸 `Ctrl`
//! (`Ctrl+Tab`、`Ctrl+1..9`、`Ctrl+,`、`` Ctrl+` ``)。
//!
//! # 与原版表的两处差异(都有意)
//!
//! 1. **多出 GPUI 独有的三条**(`ctrl-shift-a` / `u` / `j`):原版边栏没有这几个
//!    面板,描述文案是 R 批往 TS 源头补的;
//! 2. 剪贴板那两条(`Ctrl+Shift+C/V`)在原版是「xterm 的 customKeyEventHandler 消费,
//!    表里只为设置页展示」,GPUI 侧同样如此 —— 由 `mt_ui::TerminalView::on_key_down`
//!    自己吃掉,这里是 [`Scope::Terminal`],**不绑 action**(绑了就轮不到终端)。
//!    回看四条(Shift+PgUp/PgDn、Ctrl+Shift+Home/End)同一种登记法。
//!
//! 终端字号缩放(Ctrl+= / Ctrl+- / Ctrl+0)是第三种:绑成 action,但挂在 `Terminal`
//! context 上([`Scope::TerminalAction`]),只在终端有焦点时命中 —— 文件查看器等别处的
//! 同一组键不受影响。
//!
//! # macOS:应用级动作的主修饰键是 ⌘
//!
//! 表里写的是 Windows / Linux 口径(`ctrl-*`)。此前 mac 上同样绑 `ctrl-*`,而显示时把
//! 主修饰键画成 ⌘ —— 界面写着 ⌘⇧T,实际要按 Control+Shift+T。现在 mac 的默认键位按条
//! 换算([`default_keystroke_for`]):[`MacMod::Cmd`] 的条目把 `ctrl` 换成 `cmd`,
//! [`MacMod::Control`] 的条目保持 Control —— 换成 ⌘ 会撞系统键(⌘Tab 切应用、⌘` 切窗口)
//! 或终端只认 Control 的(回看键),理由写在各条表项上。纯 `alt-*` 的两边一样。
//!
//! 换算是**接受平台参数的纯函数**,`cfg` 只在 [`Platform::current`] 一处:本仓在 Windows
//! 上开发,mac 的默认键位、显示串全靠单测按 `Platform::Mac` 钉住。
//!
//! # 用户改键
//!
//! 覆盖表存在 `AppConfig::hotkey_overrides`(id → gpui 键位串,空串 = 解绑,只存与默认
//! 不同的项)。可改的只有 [`Scope::Global`] 与 [`Scope::TerminalAction`] 两类;
//! [`Scope::Terminal`] 由 mt-ui 内部消费,`Ctrl+1..9` 一条对九个键,都只读展示
//! ([`read_only_reason`])。
//!
//! **改键即时生效,走「追加」而不是「清空重建」**。读 gpui-pre 0.3.5 的 keymap 得到的事实:
//!
//! - `Keymap` 没有删单条绑定的 API,只有 `add_bindings` 与整表 `clear`
//!   (`App::clear_key_bindings`)。整表清空会连组件库的绑定一起清掉 —— 那些是
//!   `gpui_component::init` 一路注册的(Root 的 Tab 焦点导航、Input 的编辑键、Dialog 的
//!   Esc……),还有用到时才懒注册的(`menu/context_menu.rs` 首次弹出时的 `cx.bind_keys`),
//!   清了补不回来;
//! - `Keymap::bindings_for_input` 把命中的绑定按「context 深度降序、同深度后注册的在前」
//!   排序,于是**同一 context 上后注册的压过先注册的**;
//! - `NoAction` 一旦排在前面,就把排在它后面的**所有**同键绑定(含更浅 context 的、别的
//!   动作的)都屏蔽掉 —— 用来「解绑某个动作」太宽,会顺手把组件库在更浅一层的同键绑定
//!   也废了;
//! - `Unbind(动作名)` 只屏蔽排在它后面、**同键位且同动作名**的绑定,别的照旧
//!   (`binding_is_unbound`),而且 `bindings_for_action`(组件库按动作查键位的显示 API)
//!   也认它。效果恰好是「这条绑定从没注册过」。
//!
//! 所以改键 = 在同一 context 上追加两批:旧键位上一条 `Unbind(动作名)`,新键位上一条
//! 动作绑定(见 [`rebind_bindings`])。旧键位于是**真正落回**焦点所在处 —— 终端里一条
//! 绑定都匹配不上,按键走到 `TerminalView::on_key_down`;[`tests::解绑或改走之后旧键位落回终端`]
//! 拿真实的表与内部绑定建 keymap 钉住这件事。代价是 keymap 只增不减,每改一次多两条,
//! 对按键匹配的开销可以忽略。

use std::cell::RefCell;
use std::collections::BTreeMap;

use gpui::{App, KeyBinding, Keystroke, Modifiers, NoAction, Unbind};

use crate::git::{
    changes as git_changes, conflict as git_conflict, diff as git_diff,
    file_history as git_file_history, history as git_history, stash as git_stash,
};
use crate::pane::{TerminalZoomIn, TerminalZoomOut, TerminalZoomReset};
use crate::{
    ClosePane, CloseTerminalTab, FocusDown, FocusLeft, FocusRight, FocusUp, GlobalSearch,
    JumpAttention, MarkerNext, MarkerPrev, MoveTabLeft, MoveTabRight, NewTerminal, NextPane,
    OpenTerminalSettings, PrevPane, QuickOpenFile, RenamePane, ReopenClosedTerminal,
    ResizeSplitDown, ResizeSplitLeft, ResizeSplitRight, ResizeSplitUp, SelectPane, SplitDown,
    SplitRight, SwitchProject, TerminalSearch, ToggleCommandLibrary, ToggleMaximizePane,
    ToggleMiddleColumn, ToggleSessions, ToggleUsage, ToggleWorkbenchPage, command_library,
    project_switcher, quick_open, search_modal,
};

/// 应用级动作的 key context(与 `Workspace::render` 的 `key_context` 一致)。
const WORKSPACE: &str = "Workspace";

/// 终端本体的 key context(与 `mt_ui::TerminalView::render` 的 `key_context` 一致)。
const TERMINAL: &str = "Terminal";

/// `Ctrl+1..9` 那一条的 id:一条表项展开成九个绑定,只读。
const SELECT_PANE_N: &str = "selectPaneN";

/// 快捷键作用域。决定按键在什么情况下被拦截 —— 与 `hotkeys.ts::HotkeyScope` 同义。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// 全局:绑成 gpui action,终端看不到这些键。
    Global,
    /// 终端内:由 `mt_ui::TerminalView` 自己消费,这里只为设置页展示。
    Terminal,
    /// 终端内、但绑成 gpui action(挂 `Terminal` context):只在终端有焦点时命中,
    /// 命中了终端同样看不到这个键。终端字号缩放走这一档。
    TerminalAction,
}

/// macOS 上这一条的主修饰键怎么换算(见模块注释「macOS」段)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MacMod {
    /// `ctrl` 换成 `cmd`:应用级动作的默认。键位串里没有 `ctrl` 的(F2、纯 Alt)原样。
    Cmd,
    /// 保持 Control:换成 ⌘ 会撞系统键,或终端只认 Control。理由写在表项注释里。
    Control,
}

/// 默认键位按哪个平台算。`cfg` 只出现在 [`Self::current`],其余逻辑都把它当参数,
/// mac 的分支因此能在 Windows 上被单测走到。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Platform {
    Mac,
    /// Windows / Linux
    Other,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Other
        }
    }
}

pub struct HotkeyDef {
    pub id: &'static str,
    /// 默认键位:**Windows / Linux 口径**的 gpui 键位串。macOS 的默认由
    /// [`default_keystroke_for`] 按 [`Self::mac`] 换算。
    ///
    /// - `Global` / `TerminalAction`:真正绑上去的键位;
    /// - `Terminal`:不绑 action,只为设置页显示,必须与 `TerminalView::on_key_down` 的判定
    ///   一致(有单测对照);
    /// - `selectPaneN`:写九条里的第一条 `ctrl-1`,[`bind_keys`] 展开成 `1..9`,显示成 `1…9`。
    pub keystroke: &'static str,
    pub scope: Scope,
    pub mac: MacMod,
    /// 设置页分组标题的 i18n key(`settings` 命名空间内的相对 key)。
    pub group_key: &'static str,
    /// 动作描述的 i18n key(同上)。
    pub desc_key: &'static str,
}

const G_TERMINAL: &str = "shortcuts.terminalOps";
const G_NAV: &str = "shortcuts.navigation";
const G_LAYOUT: &str = "shortcuts.tabsSplits";
const G_GLOBAL: &str = "shortcuts.global";
const G_MARKER: &str = "shortcuts.aiTaskMarks";
const G_CLIPBOARD: &str = "shortcuts.clipboard";
const G_VIEW: &str = "shortcuts.scrollbackZoom";

/// 便捷构造:大多数条目只差键位与描述。
const fn def(
    id: &'static str,
    keystroke: &'static str,
    scope: Scope,
    mac: MacMod,
    group_key: &'static str,
    desc_key: &'static str,
) -> HotkeyDef {
    HotkeyDef {
        id,
        keystroke,
        scope,
        mac,
        group_key,
        desc_key,
    }
}

use MacMod::{Cmd, Control};

pub const HOTKEYS: &[HotkeyDef] = &[
    // ── 终端操作 ──
    def(
        "newTerminal",
        "ctrl-shift-t",
        Scope::Global,
        Cmd,
        G_TERMINAL,
        "shortcuts.newTerminal",
    ),
    def(
        "closePane",
        "ctrl-shift-w",
        Scope::Global,
        Cmd,
        G_TERMINAL,
        "shortcuts.closePane",
    ),
    // 关**单个**页签(= 点页签上的 ×)。Ctrl+Shift+W 关的是整组(原版语义,不改),
    // 此前单个页签没有键盘路径。终端里 Ctrl+Shift+X 在传统编码下与 Ctrl+X 同字节,
    // shell / TUI 要的那个 Ctrl+X 不受影响(与 Ctrl+Shift+O 同一条理由)
    def(
        "closeTab",
        "ctrl-shift-x",
        Scope::Global,
        Cmd,
        G_TERMINAL,
        "shortcuts.closeTab",
    ),
    def(
        "renamePane",
        "f2",
        Scope::Global,
        Cmd,
        G_TERMINAL,
        "shortcuts.renamePane",
    ),
    def(
        "splitRight",
        "ctrl-shift-d",
        Scope::Global,
        Cmd,
        G_TERMINAL,
        "shortcuts.splitRight",
    ),
    def(
        "splitDown",
        "ctrl-shift-e",
        Scope::Global,
        Cmd,
        G_TERMINAL,
        "shortcuts.splitDown",
    ),
    // 与页签栏的最大化钮、页签右键「最大化 / 还原」同一个动作(`toggle_maximized_leaf`)。
    // mac 上是 ⌘⇧M:系统的最小化是不带 Shift 的 ⌘M,不相撞
    def(
        "toggleMaximize",
        "ctrl-shift-m",
        Scope::Global,
        Cmd,
        G_TERMINAL,
        "shortcuts.toggleMaximize",
    ),
    // 重新打开已关闭的终端(`store::reopen`)。终端里 Ctrl+Shift+Z 传统编码同 Ctrl+Z,
    // shell 的挂起作业照旧按 Ctrl+Z;栈空时按了什么也不做,**不放行** —— 放行就是一次
    // SIGTSTP,比没反应糟。mac 上的 ⌘⇧Z 是输入框的「重做」,与 Windows 上 Ctrl+Shift+Z
    // 的处境一样:输入框自己的绑定挂在更深的 `Input` 上,先轮到它
    def(
        "reopenClosed",
        "ctrl-shift-z",
        Scope::Global,
        Cmd,
        G_TERMINAL,
        "shortcuts.reopenClosed",
    ),
    // ── 导航 ──
    //
    // Ctrl+Tab / Ctrl+Shift+Tab 在 mac 上**保持 Control**:⌘Tab / ⌘⇧Tab 是系统的切换应用,
    // 根本到不了这里
    def(
        "nextPane",
        "ctrl-tab",
        Scope::Global,
        Control,
        G_NAV,
        "shortcuts.nextPane",
    ),
    def(
        "prevPane",
        "ctrl-shift-tab",
        Scope::Global,
        Control,
        G_NAV,
        "shortcuts.prevPane",
    ),
    // 一条表项 → 九个绑定,键位串写第一条,由 bind_keys 展开(原版这里存的是占位串
    // `'1…9'`)。mac 上是 ⌘1..9(浏览器 / 编辑器切标签的惯例)
    def(
        SELECT_PANE_N,
        "ctrl-1",
        Scope::Global,
        Cmd,
        G_NAV,
        "shortcuts.selectPaneN",
    ),
    // Alt+方向键:那个方向没有相邻分屏时处理器 `propagate`,按键落回终端(fish / zsh 与
    // 不少 TUI 拿它按词移动),见 `main.rs::Workspace::focus_adjacent`
    def(
        "focusLeft",
        "alt-left",
        Scope::Global,
        Cmd,
        G_NAV,
        "shortcuts.focusLeft",
    ),
    def(
        "focusRight",
        "alt-right",
        Scope::Global,
        Cmd,
        G_NAV,
        "shortcuts.focusRight",
    ),
    def(
        "focusUp",
        "alt-up",
        Scope::Global,
        Cmd,
        G_NAV,
        "shortcuts.focusUp",
    ),
    def(
        "focusDown",
        "alt-down",
        Scope::Global,
        Cmd,
        G_NAV,
        "shortcuts.focusDown",
    ),
    // 终端页 ↔ 文件页(VS Code 开合终端面板的那个键)。裸 Ctrl 也不吃 shell 的键:终端的编码
    // 表里 Ctrl+` 本来就不发任何字节(`mt_ui::terminal::input::control_code` 不认 `` ` ``)。
    // mac 上**保持 Control**:⌘` 是系统的「同一应用的窗口间切换」
    def(
        "toggleWorkbenchPage",
        "ctrl-`",
        Scope::Global,
        Control,
        G_NAV,
        "shortcuts.toggleWorkbenchPage",
    ),
    // ── 页签与分屏 ──
    //
    // 页签左右移:只在所属分组内换位,到头不循环(`tree::shifted_index`)。不与回看的
    // Shift+PgUp/PgDn 相撞:那两条要求不带 Ctrl(`mt_ui::terminal::scrollback_key`)
    def(
        "moveTabLeft",
        "ctrl-shift-pageup",
        Scope::Global,
        Cmd,
        G_LAYOUT,
        "shortcuts.moveTabLeft",
    ),
    def(
        "moveTabRight",
        "ctrl-shift-pagedown",
        Scope::Global,
        Cmd,
        G_LAYOUT,
        "shortcuts.moveTabRight",
    ),
    // 调分屏比例:把当前 pane 那一侧最近的分隔线朝箭头方向推一步(pane 朝那边变大)。
    // 那个方向没有分隔线时处理器 `propagate`,按键落回终端 —— 与 Alt+方向键同一做法,
    // 见 `main.rs::Workspace::resize_split`
    def(
        "resizeLeft",
        "alt-shift-left",
        Scope::Global,
        Cmd,
        G_LAYOUT,
        "shortcuts.resizeLeft",
    ),
    def(
        "resizeRight",
        "alt-shift-right",
        Scope::Global,
        Cmd,
        G_LAYOUT,
        "shortcuts.resizeRight",
    ),
    def(
        "resizeUp",
        "alt-shift-up",
        Scope::Global,
        Cmd,
        G_LAYOUT,
        "shortcuts.resizeUp",
    ),
    def(
        "resizeDown",
        "alt-shift-down",
        Scope::Global,
        Cmd,
        G_LAYOUT,
        "shortcuts.resizeDown",
    ),
    // ── 全局 ──
    def(
        "switchProject",
        "ctrl-shift-p",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.switchProject",
    ),
    def(
        "globalSearch",
        "ctrl-shift-f",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.toggleGlobalSearch",
    ),
    // 快速打开文件(VS Code 的 Ctrl+P)。裸 Ctrl+P 是 shell 的上一条历史,按开头的原则走
    // Ctrl+Shift;Ctrl+Shift+P 已是项目切换器,取「Open」的 O。终端里的 Ctrl+Shift+O 在传统
    // 编码下与 Ctrl+O 同字节,shell / TUI 要的那个 Ctrl+O 不受影响
    def(
        "quickOpen",
        "ctrl-shift-o",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.quickOpen",
    ),
    // mac 上是 ⌘F,把 Control+F(Emacs 的 forward-char)还给了 shell
    def(
        "terminalSearch",
        "ctrl-f",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.terminalSearch",
    ),
    // mac 上是 ⌘,(「偏好设置」的系统惯例)
    def(
        "openSettings",
        "ctrl-,",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.openSettings",
    ),
    def(
        "toggleSidebar",
        "ctrl-shift-b",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.toggleSidebar",
    ),
    // GPUI 独有的三条(原版边栏没有这几个面板)
    def(
        "toggleSessions",
        "ctrl-shift-a",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.toggleSessions",
    ),
    def(
        "toggleUsage",
        "ctrl-shift-u",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.toggleUsage",
    ),
    def(
        "jumpAttention",
        "ctrl-shift-j",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.jumpAttention",
    ),
    // 命令库(issue #81)。裸 Ctrl+K 在 bash 里是 kill-line,按开头的原则走 Ctrl+Shift
    def(
        "commandLibrary",
        "ctrl-shift-k",
        Scope::Global,
        Cmd,
        G_GLOBAL,
        "shortcuts.commandLibrary",
    ),
    // ── AI 任务标记(`hotkeys.ts:73-74`)──
    //
    // 键名 `up`/`down` 与项目切换器那两条一致。原版这两条**不走 useGlobalHotkeys**
    // (那边显式把它们排除),自己挂 capture 阶段的 window 监听,于是绕过了
    // `isTypingTarget` 与 overlay 两道闸;GPUI 侧照常走 `yields_to_overlay` ——
    // 方向键在输入框里有明确语义,在设置对话框里按 Ctrl+Shift+↑ 跳终端是意外行为。
    // 这是刻意偏差,见 `main.rs::Workspace::on_marker_prev`。
    //
    // mac 上是 ⌘⇧↑ / ⌘⇧↓:Control+↑ / ↓ 是系统的调度中心 / 应用窗口,带不带 Shift 都别碰;
    // 终端对 ⌘ 组合不发字节,PR #99 接管的也只是不带 Shift 的 ⌘← / ⌘→
    def(
        "markerPrev",
        "ctrl-shift-up",
        Scope::Global,
        Cmd,
        G_MARKER,
        "shortcuts.jumpPrevAi",
    ),
    def(
        "markerNext",
        "ctrl-shift-down",
        Scope::Global,
        Cmd,
        G_MARKER,
        "shortcuts.jumpNextAi",
    ),
    // ── 剪贴板(终端内,由 TerminalView 消费;这里只为设置页展示)──
    //
    // `TerminalView::on_key_down` 认的是 `(control || platform) && shift` + c / v,两个修饰键
    // 同权。mac 上显示 ⌘⇧C / ⌘⇧V(⌃⇧C / ⌃⇧V 也照样能用,判冲突时一并算终端占用)
    def(
        "copySelection",
        "ctrl-shift-c",
        Scope::Terminal,
        Cmd,
        G_CLIPBOARD,
        "shortcuts.copySelected",
    ),
    def(
        "pasteToTerminal",
        "ctrl-shift-v",
        Scope::Terminal,
        Cmd,
        G_CLIPBOARD,
        "shortcuts.pasteToTerminal",
    ),
    // ── 回看与缩放(终端内)──
    //
    // 回看四条由 `mt_ui::TerminalView` 自己消费(判据 `mt_ui::terminal::scrollback_key`,
    // 只在主屏拦,alt screen 照旧发给程序),与剪贴板那两条同一种登记法。
    // 不占 Shift+Home / Shift+End:PSReadLine 拿它们选到行首 / 行尾。
    // Ctrl+Shift+Home / End 在 mac 上**保持 Control**:`scrollback_key` 遇到 `platform`
    // 位直接不认,显示成 ⌘ 就又是「写着能按、按了没反应」
    def(
        "scrollPageUp",
        "shift-pageup",
        Scope::Terminal,
        Control,
        G_VIEW,
        "shortcuts.scrollPageUp",
    ),
    def(
        "scrollPageDown",
        "shift-pagedown",
        Scope::Terminal,
        Control,
        G_VIEW,
        "shortcuts.scrollPageDown",
    ),
    def(
        "scrollTop",
        "ctrl-shift-home",
        Scope::Terminal,
        Control,
        G_VIEW,
        "shortcuts.scrollTop",
    ),
    def(
        "scrollBottom",
        "ctrl-shift-end",
        Scope::Terminal,
        Control,
        G_VIEW,
        "shortcuts.scrollBottom",
    ),
    // 缩放三条:action 挂在 `Terminal` context 上(见模块注释),落点在
    // `pane::zoom_terminal_font`,改的是全局终端字号。Ctrl+滚轮同效,写在描述里
    // (滚轮那一路各平台都认 Control,见 `mt_ui::terminal::element` 的 `on_zoom`)。
    // mac 上是 ⌘= / ⌘- / ⌘0(浏览器 / 编辑器缩放的惯例)
    def(
        "zoomIn",
        "ctrl-=",
        Scope::TerminalAction,
        Cmd,
        G_VIEW,
        "shortcuts.zoomIn",
    ),
    def(
        "zoomOut",
        "ctrl--",
        Scope::TerminalAction,
        Cmd,
        G_VIEW,
        "shortcuts.zoomOut",
    ),
    def(
        "zoomReset",
        "ctrl-0",
        Scope::TerminalAction,
        Cmd,
        G_VIEW,
        "shortcuts.zoomReset",
    ),
];

/// 智能 Ctrl+C / Ctrl+V 开着时附在「复制粘贴」组末尾的两行(`(描述 key, 键位串)`)。
/// 由 `TerminalView` 的 `smart_key_action` 消费,不进表、不可改。mac 上显示 ⌃C / ⌃V:
/// 开关管的是 Control 那一路(⌘C / ⌘V 在 mac 上不受开关管、始终是复制粘贴)。
pub const SMART_COPY_PASTE: [(&str, &str); 2] = [
    ("shortcuts.copyDesc", "ctrl-c"),
    ("shortcuts.pasteToTerminal", "ctrl-v"),
];

/// 按 id 找表项。
pub fn find(id: &str) -> Option<&'static HotkeyDef> {
    HOTKEYS.iter().find(|def| def.id == id)
}

/// 设置页用:按 `group_key` 归组,保持表内声明顺序
/// (`hotkeys.ts::hotkeyGroups` 同一算法)。
pub fn groups() -> Vec<(&'static str, Vec<&'static HotkeyDef>)> {
    let mut out: Vec<(&'static str, Vec<&'static HotkeyDef>)> = Vec::new();
    for def in HOTKEYS {
        match out.iter_mut().find(|(key, _)| *key == def.group_key) {
            Some((_, items)) => items.push(def),
            None => out.push((def.group_key, vec![def])),
        }
    }
    out
}

// ─── 默认键位(按平台) ─────────────────────────────────────────

/// 这一条在 `platform` 上的默认键位串(规范形,见 [`normalize_keystroke`])。
pub fn default_keystroke_for(def: &HotkeyDef, platform: Platform) -> String {
    let parsed = Keystroke::parse(def.keystroke).expect("键位表里的键位串必须解析得了");
    let mut modifiers = parsed.modifiers;
    if platform == Platform::Mac && def.mac == MacMod::Cmd && modifiers.control {
        modifiers.control = false;
        modifiers.platform = true;
    }
    canonical(&modifiers, &parsed.key)
}

/// `Ctrl+1..9`(mac 上 ⌘1..9)展开后的九条:`(序号, 键位串)`。
fn digit_keystrokes(platform: Platform) -> Vec<(usize, String)> {
    let def = find(SELECT_PANE_N).expect("selectPaneN 在表里");
    let first = default_keystroke_for(def, platform);
    let prefix = first
        .strip_suffix('1')
        .expect("selectPaneN 的键位串以 1 结尾");
    (1..=9).map(|n| (n, format!("{prefix}{n}"))).collect()
}

// ─── 键位串:规范形与显示串 ────────────────────────────────────

/// 规范形:修饰键按 `fn-ctrl-alt-cmd-shift-` 的固定次序(与 gpui `unparse` 同序,只是
/// `platform` 一律写 `cmd` —— gpui 的 parse 认 cmd / super / win 三种写法,落盘只留一种)。
/// 表里的默认键位本来就是这个形状(有单测钉住),比较「是不是同一个键」直接比字符串。
fn canonical(modifiers: &Modifiers, key: &str) -> String {
    let mut out = String::new();
    if modifiers.function {
        out.push_str("fn-");
    }
    if modifiers.control {
        out.push_str("ctrl-");
    }
    if modifiers.alt {
        out.push_str("alt-");
    }
    if modifiers.platform {
        out.push_str("cmd-");
    }
    if modifiers.shift {
        out.push_str("shift-");
    }
    out.push_str(key);
    out
}

fn os_of(platform: Platform) -> gpui::OperatingSystem {
    match platform {
        Platform::Mac => gpui::OperatingSystem::Mac,
        Platform::Other => {
            if cfg!(target_os = "linux") {
                gpui::OperatingSystem::Linux
            } else {
                gpui::OperatingSystem::Windows
            }
        }
    }
}

/// 把一个键位串规范化。解析不了、是多键序列(`ctrl-k ctrl-s`)时返回 `None`。
pub fn normalize_keystroke(keystroke: &str) -> Option<String> {
    let keystroke = keystroke.trim();
    if keystroke.is_empty() || keystroke.split_whitespace().count() != 1 {
        return None;
    }
    let parsed = Keystroke::parse_with_os(keystroke, os_of(Platform::current())).ok()?;
    Some(canonical(&parsed.modifiers, &parsed.key))
}

/// gpui 键位串里的键名 → 显示串(`hotkeys.ts::KEY_LABELS` 的等价物)。
pub fn key_label(key: &str) -> String {
    match key {
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "tab" => "Tab".into(),
        "escape" => "Esc".into(),
        "enter" => "Enter".into(),
        "space" => "Space".into(),
        "backspace" => "Backspace".into(),
        "delete" => "Delete".into(),
        "insert" => "Insert".into(),
        "home" => "Home".into(),
        "end" => "End".into(),
        // 与回看那两条的显示串同一种缩写(`PgUp` / `PgDn`)
        "pageup" => "PgUp".into(),
        "pagedown" => "PgDn".into(),
        // 单字符键与 F1..F24 一律大写(`t` → `T`,`f2` → `F2`)
        other if other.chars().count() == 1 || is_function_key(other, 24) => other.to_uppercase(),
        // 其余具名键首字母大写(`menu` → `Menu`)
        other => {
            let mut chars = other.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

/// `f1`..`f{max}`。
fn is_function_key(key: &str, max: u32) -> bool {
    key.strip_prefix('f')
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| (1..=max).contains(&n))
}

/// 修饰键 + 键名 → 显示串。Windows / Linux 上 `Ctrl+Shift+T`;macOS 上 `⌘⇧T`(符号直接拼、
/// 不加分隔符,沿用 `hotkeys.ts::comboLabel` 的风格),Control 画成 ⌃ —— 此前 mac 上两者
/// 都画成 ⌘,正是「显示 ⌘、实际要按 Control」那个错的来源。
///
/// 次序固定为 Ctrl / ⌃ → ⌘ → Shift → Alt,与改造前的 `combo_label` 一字不差
/// (Windows 上 `Shift+Alt+←` 这类串不变)。
fn modifiers_label(modifiers: &Modifiers, key: &str, platform: Platform) -> String {
    let mac = platform == Platform::Mac;
    let mut parts: Vec<&str> = Vec::new();
    if modifiers.control {
        parts.push(if mac { "⌃" } else { "Ctrl" });
    }
    if modifiers.platform {
        // Windows 上是 Win 键;录制与读配置都不收它(见 [`Invalid::SystemKey`]),只是兜底
        parts.push(if mac { "⌘" } else { "Win" });
    }
    if modifiers.function {
        parts.push("Fn");
    }
    if modifiers.shift {
        parts.push(if mac { "⇧" } else { "Shift" });
    }
    if modifiers.alt {
        parts.push(if mac { "⌥" } else { "Alt" });
    }
    parts.push(key);
    parts.join(if mac { "" } else { "+" })
}

/// 一个 gpui 键位串的显示串。解析不了就原样返回(只会是手改坏的配置,不炸)。
pub fn keystroke_label_for(keystroke: &str, platform: Platform) -> String {
    match Keystroke::parse_with_os(keystroke, os_of(platform)) {
        Ok(parsed) => modifiers_label(&parsed.modifiers, &key_label(&parsed.key), platform),
        Err(_) => keystroke.to_string(),
    }
}

/// 当前平台上一个键位串的显示串。
pub fn keystroke_label(keystroke: &str) -> String {
    keystroke_label_for(keystroke, Platform::current())
}

/// 表项的显示串:`selectPaneN` 把键名换成 `1…9`,其余就是键位串本身的显示串。
fn def_label(def: &HotkeyDef, keystroke: &str, platform: Platform) -> String {
    if def.id != SELECT_PANE_N {
        return keystroke_label_for(keystroke, platform);
    }
    match Keystroke::parse_with_os(keystroke, os_of(platform)) {
        Ok(parsed) => modifiers_label(&parsed.modifiers, "1…9", platform),
        Err(_) => keystroke.to_string(),
    }
}

// ─── 可改性 ──────────────────────────────────────────────────

/// 能不能在设置页里改。
pub fn is_editable(def: &HotkeyDef) -> bool {
    matches!(def.scope, Scope::Global | Scope::TerminalAction) && def.id != SELECT_PANE_N
}

/// 不可改的条目为什么不可改(设置页那行的 tooltip,`settings` 命名空间内的 i18n key)。
/// 可改的返回 `None`。
pub fn read_only_reason(def: &HotkeyDef) -> Option<&'static str> {
    if def.scope == Scope::Terminal {
        Some("shortcuts.readOnlyTerminal")
    } else if def.id == SELECT_PANE_N {
        Some("shortcuts.readOnlyDigits")
    } else {
        None
    }
}

// ─── 覆盖表 ──────────────────────────────────────────────────

/// 校验过的覆盖表:只含认得出的、可改的 id;值 `None` = 解绑。
///
/// 配置里存的是原始的 `BTreeMap<String, String>`(见 `AppConfig::hotkey_overrides`),
/// 读进来过一遍 [`Self::parse`] 才用 —— 手改坏的、旧版本认不出的,到这里就丢了。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Overrides(BTreeMap<&'static str, Option<String>>);

impl Overrides {
    /// 从配置里的原始表读出来。认不出的 id、不许改的条目、解析不了或不合法的键位串一律
    /// 丢弃,在返回的说明里记一笔(调用方写日志)—— **只是这次不用,不回写配置**:
    /// 认不出的 id 多半是新版本加的,旧版本顺手删掉就把它弄丢了(`config.db` 前向兼容
    /// 同一口径)。
    pub fn parse(raw: &BTreeMap<String, String>, platform: Platform) -> (Self, Vec<String>) {
        let mut out = BTreeMap::new();
        let mut issues = Vec::new();
        for (id, value) in raw {
            let Some(def) = find(id) else {
                issues.push(format!("忽略认不出的快捷键 id「{id}」"));
                continue;
            };
            if !is_editable(def) {
                issues.push(format!("忽略不可更改的条目「{id}」的覆盖"));
                continue;
            }
            if value.trim().is_empty() {
                out.insert(def.id, None);
                continue;
            }
            let Some(parsed) = parse_single(value) else {
                issues.push(format!(
                    "快捷键「{id}」的键位串「{value}」读不懂,按默认键位"
                ));
                continue;
            };
            if let Some(problem) = combo_problem(&parsed.modifiers, &parsed.key, None, platform) {
                issues.push(format!(
                    "快捷键「{id}」的键位串「{value}」不合法({problem:?}),按默认键位"
                ));
                continue;
            }
            out.insert(def.id, Some(canonical(&parsed.modifiers, &parsed.key)));
        }
        (Self(out), issues)
    }

    /// 有没有任何一条改过(设置页「全部恢复默认」出不出)。
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// 只解析单个键位(多键序列不收)。
fn parse_single(keystroke: &str) -> Option<Keystroke> {
    let keystroke = keystroke.trim();
    if keystroke.split_whitespace().count() != 1 {
        return None;
    }
    Keystroke::parse_with_os(keystroke, os_of(Platform::current())).ok()
}

/// 这一条此刻生效的键位:覆盖优先,否则默认,解绑为 `None`。不可改的条目永远是默认。
pub fn effective_keystroke_for(
    def: &HotkeyDef,
    overrides: &Overrides,
    platform: Platform,
) -> Option<String> {
    if !is_editable(def) {
        return Some(default_keystroke_for(def, platform));
    }
    match overrides.0.get(def.id) {
        Some(value) => value.clone(),
        None => Some(default_keystroke_for(def, platform)),
    }
}

/// 这一条此刻的显示串;解绑了是 `None`。
pub fn label_for(def: &HotkeyDef, overrides: &Overrides, platform: Platform) -> Option<String> {
    effective_keystroke_for(def, overrides, platform).map(|ks| def_label(def, &ks, platform))
}

/// 这一条被改过没有(设置页的「已修改」标记与单行「恢复默认」)。
pub fn is_modified(def: &HotkeyDef, overrides: &Overrides, platform: Platform) -> bool {
    effective_keystroke_for(def, overrides, platform) != Some(default_keystroke_for(def, platform))
}

/// 把一次改键写进配置里的原始覆盖表。`keystroke = None` = 解绑(存空串)。
/// 与默认相同就删掉那一项(只存与默认不同的项);认不出 / 不可改的 id 什么也不做。
pub fn write_override(
    raw: &mut BTreeMap<String, String>,
    id: &str,
    keystroke: Option<&str>,
    platform: Platform,
) {
    let Some(def) = find(id).filter(|def| is_editable(def)) else {
        return;
    };
    let value = match keystroke {
        None => String::new(),
        Some(keystroke) => match normalize_keystroke(keystroke) {
            Some(value) => value,
            None => return,
        },
    };
    if value == default_keystroke_for(def, platform) {
        raw.remove(def.id);
    } else {
        raw.insert(def.id.to_string(), value);
    }
}

/// 恢复默认:`Some(id)` 只恢复那一条,`None` 恢复全部。「全部」只清本版本认得的可改条目,
/// 认不出的原样留着(同 [`Overrides::parse`] 的前向兼容口径)。
pub fn reset_overrides(raw: &mut BTreeMap<String, String>, id: Option<&str>) {
    raw.retain(|key, _| {
        let known = find(key).is_some_and(is_editable);
        let targeted = id.is_none_or(|id| id == key);
        !(known && targeted)
    });
}

// ─── 运行期:此刻绑着的是哪一份 ──────────────────────────────

thread_local! {
    /// 此刻 keymap 里生效的覆盖表。只在 [`bind_keys`] / [`apply_overrides`] 里改 —— 与往
    /// gpui 压绑定是**同一处**,所以按它推出来的显示串就是真正绑着的键位。
    ///
    /// `thread_local` 而不是 gpui `Global`:与 [`crate::overlay`] 的栈同一个理由,视图全在
    /// 主线程上,而且取显示串的地方(菜单项、tooltip 构造)不必再带 `&App`;单测天然隔离。
    static APPLIED: RefCell<Overrides> = RefCell::new(Overrides::default());
}

/// 此刻生效的覆盖表(设置页判冲突、画「已修改」用)。
pub fn applied_overrides() -> Overrides {
    APPLIED.with(|applied| applied.borrow().clone())
}

/// 按 id 取一条快捷键**此刻**的显示串(`hotkeys.ts::hotkeyLabel`)。
///
/// 表里没有这个 id、或用户把它解绑了,返回空串 —— 原版那边是 `hotkeys[id]` 取
/// `undefined` 再拼成空,同样不炸。菜单项的快捷键列拿到空串就不画
/// (`menu::MenuItem::shortcut`);带括号的 tooltip 改用 [`hotkey_tip`]。
pub fn hotkey_label(id: &str) -> String {
    let Some(def) = find(id) else {
        return String::new();
    };
    APPLIED
        .with(|applied| label_for(def, &applied.borrow(), Platform::current()))
        .unwrap_or_default()
}

/// tooltip 里那个 `{shortcut}` 的取值:有键位就是显示串,解绑了是「未设置」——
/// 「在终端中查找(未设置)」比留一对空括号好认。
pub fn hotkey_tip(id: &str) -> String {
    let label = hotkey_label(id);
    if label.is_empty() {
        crate::i18n::t("settings", "shortcuts.unbound").to_string()
    } else {
        label
    }
}

/// 按 id 取一条快捷键的**描述**文案 key(`settings` 命名空间内的相对 key)。
///
/// 首启引导那三条提示原版是手写 `t('settings.shortcuts.newTerminal')` 之类
/// (`FirstRunGuide.tsx:36-40`),这里改从表里取同一个 `desc_key` —— 串完全一样,
/// 但改键位表时不会漏掉引导页。
pub fn hotkey_desc_key(id: &str) -> Option<&'static str> {
    find(id).map(|def| def.desc_key)
}

// ─── 录制:一次按键怎么算 ─────────────────────────────────────

/// 设置页录制时收到的一次按键怎么处理(见 [`interpret_recorded`])。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recorded {
    /// 只按了修饰键:不算一次完成的录制,接着等。
    Pending,
    /// 裸 Esc:取消录制,键位不动。
    Cancel,
    /// 裸 Backspace / Delete:解绑。
    Unbind,
    /// 不合法:就地说明原因,留在录制态。
    Invalid(Invalid),
    /// 合法的组合键(规范形)。还要过 [`judge`] 判冲突。
    Combo(String),
}

/// 不收的组合键,及其原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invalid {
    /// 没带 Ctrl / Alt(mac 上还有 ⌘),也不是 F1–F12:裸字母、裸 Enter / Tab / Esc /
    /// 空格、仅 Shift+字母 —— 绑上就是终端与输入法收不到这个键。
    NeedsModifier,
    /// Windows / Linux 上带 Win(Super)键:那是系统的键,多数到不了应用。
    SystemKey,
    /// 系统或输入法占用的组合(见 [`RESERVED_OTHER`] / [`RESERVED_MAC`])。
    Reserved,
    /// Windows 上 Ctrl+Alt+键打出了字符 —— 那是 AltGr(德语键盘的 `@` 就是 AltGr+Q),
    /// 绑上之后这个字符就打不进终端了。
    TypesCharacter,
}

impl Invalid {
    /// 就地提示的 i18n key(`settings` 命名空间内)。
    pub fn message_key(self) -> &'static str {
        match self {
            Self::NeedsModifier => "shortcuts.invalidNeedsModifier",
            Self::SystemKey => "shortcuts.invalidSystemKey",
            Self::Reserved => "shortcuts.invalidReserved",
            Self::TypesCharacter => "shortcuts.invalidTypesChar",
        }
    }
}

/// Windows / Linux 上系统或输入法占用的组合(规范形)。多数根本到不了应用(系统先截走),
/// 列出来是防「系统那边关了这个快捷键」时录进去,换台机器就失灵。
///
/// - `alt-tab` 一族:切窗口;`alt-f4`:关窗口;`alt-space`:窗口菜单;`alt-escape`:循环窗口;
/// - `ctrl-escape`:开始菜单;`ctrl-shift-escape`:任务管理器;`ctrl-alt-delete`:安全选项;
/// - `ctrl-space`:中文输入法的中英切换(微软拼音等的默认键)。
const RESERVED_OTHER: &[&str] = &[
    "alt-tab",
    "alt-shift-tab",
    "ctrl-alt-tab",
    "alt-f4",
    "alt-space",
    "alt-escape",
    "ctrl-escape",
    "ctrl-shift-escape",
    "ctrl-alt-delete",
    "ctrl-space",
];

/// macOS 上系统或输入法占用的组合(规范形)。
///
/// - ⌘Tab / ⌘⇧Tab:切应用;⌘`:同一应用的窗口间切换;⌘空格 / ⌃空格 / ⌥⌘空格:Spotlight
///   与输入法切换;⌃⌘空格:表情与符号;
/// - ⌘Q / ⌘H / ⌥⌘H / ⌘M / ⌘W:退出 / 隐藏 / 隐藏其他 / 最小化 / 关窗口 —— 菜单栏惯例,
///   用户的肌肉记忆比哪个动作都硬;⌥⌘Esc:强制退出;⌃⌘Q:锁屏;⌃⌘F:全屏;
/// - ⌃↑ / ⌃↓ / ⌃← / ⌃→:调度中心、应用窗口与切换桌面。
const RESERVED_MAC: &[&str] = &[
    "cmd-tab",
    "cmd-shift-tab",
    "cmd-`",
    "cmd-space",
    "ctrl-space",
    "alt-cmd-space",
    "ctrl-cmd-space",
    "cmd-q",
    "cmd-h",
    "alt-cmd-h",
    "cmd-m",
    "cmd-w",
    "alt-cmd-escape",
    "ctrl-cmd-q",
    "ctrl-cmd-f",
    "ctrl-up",
    "ctrl-down",
    "ctrl-left",
    "ctrl-right",
];

fn reserved(platform: Platform) -> &'static [&'static str] {
    match platform {
        Platform::Mac => RESERVED_MAC,
        Platform::Other => RESERVED_OTHER,
    }
}

/// gpui 报给「只按了修饰键」的键名(`Window::dispatch_key_event` 合成的 `shift` / `control`
/// 之类)。Windows / mac 后端按住修饰键只发 `ModifiersChanged`,到不了录制回调;这里兜一道。
fn is_modifier_key(key: &str) -> bool {
    matches!(
        key,
        "" | "shift" | "control" | "alt" | "platform" | "function" | "capslock"
    )
}

/// 一个组合键合不合法(录制与读配置共用的那套规则)。`key_char` 只有录制时才有。
fn combo_problem(
    modifiers: &Modifiers,
    key: &str,
    key_char: Option<&str>,
    platform: Platform,
) -> Option<Invalid> {
    let mac = platform == Platform::Mac;
    if is_modifier_key(key) {
        return Some(Invalid::NeedsModifier);
    }
    if modifiers.platform && !mac {
        return Some(Invalid::SystemKey);
    }
    let primary = modifiers.control || modifiers.alt || modifiers.platform;
    if !primary && !is_function_key(key, 12) {
        return Some(Invalid::NeedsModifier);
    }
    if !mac && modifiers.control && modifiers.alt && key_char.is_some_and(|c| !c.is_empty()) {
        return Some(Invalid::TypesCharacter);
    }
    if reserved(platform).contains(&canonical(modifiers, key).as_str()) {
        return Some(Invalid::Reserved);
    }
    None
}

/// 录制时收到的一次按键怎么算。拿的是 gpui 报上来的原样 `Keystroke`:绑定匹配比的也是
/// 它(修饰键 + 键名),原样存下去,下次同一个物理按键就一定匹配得上。
pub fn interpret_recorded(keystroke: &Keystroke, platform: Platform) -> Recorded {
    let modifiers = &keystroke.modifiers;
    let key = keystroke.key.as_str();
    if is_modifier_key(key) {
        return Recorded::Pending;
    }
    let bare = !(modifiers.control
        || modifiers.alt
        || modifiers.shift
        || modifiers.platform
        || modifiers.function);
    if bare {
        match key {
            "escape" => return Recorded::Cancel,
            "backspace" | "delete" => return Recorded::Unbind,
            _ => {}
        }
    }
    match combo_problem(modifiers, key, keystroke.key_char.as_deref(), platform) {
        Some(problem) => Recorded::Invalid(problem),
        None => Recorded::Combo(canonical(modifiers, key)),
    }
}

// ─── 录制:冲突 ──────────────────────────────────────────────

/// 录到的键位已经有主了,而且主不能让(设置页直接拒绝)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// 终端自己消费的键(只读条目、`Ctrl+1..9`、智能复制粘贴、mac 的 ⌘← / ⌘→)。
    /// 值是那个用途的描述 key(`settings` 命名空间内)。
    Named(&'static str),
    /// 弹窗 / 面板 / 页签内部的键位(不进表的那些)。
    Internal,
}

/// 一次录制结果的裁决(见 [`judge`])。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 与这一条此刻的键位相同:什么都不用做。
    Unchanged,
    /// 被不能让的占着:拒绝并说明。
    Taken(Owner),
    /// 被另一条可改的占着:给「替换(那一条解绑)/ 取消」。值是那一条的 id。
    Conflict(&'static str),
    /// 可以保存。
    Accept,
}

/// 文件页 / diff 页签挂在容器 `on_key_down` 上的键(不是绑定,从 keymap 里推不出来):
/// `Ctrl/Cmd+W` 关页签、`Ctrl/Cmd+S` 保存(`file_viewer/mod.rs`、`git_diff/tab.rs`,
/// 判据是 `Modifiers::secondary()`,mac 上是 ⌘、其余是 Ctrl)。全局绑定先于 key 监听派发,
/// 绑了它们,文件页上的保存 / 关闭就没了。
fn listener_keys(platform: Platform) -> &'static [&'static str] {
    match platform {
        Platform::Mac => &["cmd-w", "cmd-s"],
        Platform::Other => &["ctrl-w", "ctrl-s"],
    }
}

/// 组件库自己注册、又可能被录到的键:检查器开关(`gpui_component::inspector`,不挂
/// context,= 最深一层,谁也压不过它)与 Root 的复制。
fn component_keys(platform: Platform) -> &'static [&'static str] {
    match platform {
        Platform::Mac => &["alt-cmd-i", "cmd-c"],
        Platform::Other => &["ctrl-shift-i", "ctrl-c"],
    }
}

/// 弹窗 / 面板 / 页签内部的键位(规范形):从 [`internal_bindings`] **真正绑上去的那份**推
/// (不另抄清单),再加上 key 监听与组件库的那几个。
fn internal_keystrokes(platform: Platform) -> Vec<String> {
    let mut out: Vec<String> = internal_bindings()
        .iter()
        .filter(|binding| !gpui::is_no_action(binding.action()))
        .filter_map(|binding| match binding.keystrokes() {
            [one] => Some(canonical(&one.inner().modifiers, &one.inner().key)),
            _ => None,
        })
        .collect();
    out.extend(listener_keys(platform).iter().map(|ks| ks.to_string()));
    out.extend(component_keys(platform).iter().map(|ks| ks.to_string()));
    out
}

/// 终端自己消费的键位 → 用途的描述 key。`smart_copy_paste` 是设置里「智能 Ctrl+C/V」的开关。
fn terminal_owner(
    candidate: &str,
    platform: Platform,
    smart_copy_paste: bool,
) -> Option<&'static str> {
    for def in HOTKEYS.iter().filter(|def| def.scope == Scope::Terminal) {
        if default_keystroke_for(def, platform) == candidate {
            return Some(def.desc_key);
        }
        // `TerminalView` 的剪贴板分支 Control 与 ⌘ 同权:mac 上表里显示 ⌘⇧C,⌃⇧C 一样被它吃掉
        if platform == Platform::Mac && def.mac == MacMod::Cmd && def.keystroke == candidate {
            return Some(def.desc_key);
        }
    }
    if digit_keystrokes(platform)
        .iter()
        .any(|(_, ks)| ks == candidate)
    {
        return Some("shortcuts.selectPaneN");
    }
    if smart_copy_paste
        && let Some((desc, _)) = SMART_COPY_PASTE.iter().find(|(_, ks)| *ks == candidate)
    {
        return Some(desc);
    }
    if platform == Platform::Mac {
        // mac 上 ⌘C / ⌘V 不受智能开关管、始终是复制粘贴(`smart_key_action`);
        // ⌘← / ⌘→ 被翻成 Ctrl+A / Ctrl+E 跳行首行尾(PR #99,`keystroke_to_bytes`)
        match candidate {
            "cmd-c" => return Some("shortcuts.copyDesc"),
            "cmd-v" => return Some("shortcuts.pasteToTerminal"),
            "cmd-left" | "cmd-right" => return Some("shortcuts.lineStartEnd"),
            _ => {}
        }
    }
    None
}

/// 给 `id` 录到了 `candidate`(规范形),该怎么办。
///
/// - 与它此刻的键位相同 → [`Verdict::Unchanged`];
/// - 终端自己消费的键 / 弹窗内部键位 → [`Verdict::Taken`],直接拒绝;
///   例外:录回**它自己的默认键位**不算撞内部键位(Alt+← 本来就与 diff 页签的 Alt+← 并存);
/// - 另一条可改条目此刻的键位 → [`Verdict::Conflict`]。可改的两类(`Workspace` 上的全局
///   动作、`Terminal` 上的缩放)在终端有焦点时都会命中,一律按冲突算;
/// - 其余 → [`Verdict::Accept`]。
pub fn judge(
    id: &str,
    candidate: &str,
    overrides: &Overrides,
    platform: Platform,
    smart_copy_paste: bool,
) -> Verdict {
    let Some(def) = find(id) else {
        return Verdict::Unchanged;
    };
    if effective_keystroke_for(def, overrides, platform).as_deref() == Some(candidate) {
        return Verdict::Unchanged;
    }
    if let Some(desc) = terminal_owner(candidate, platform, smart_copy_paste) {
        return Verdict::Taken(Owner::Named(desc));
    }
    if candidate != default_keystroke_for(def, platform)
        && internal_keystrokes(platform)
            .iter()
            .any(|ks| ks == candidate)
    {
        return Verdict::Taken(Owner::Internal);
    }
    let taken_by = HOTKEYS.iter().find(|other| {
        other.id != def.id
            && is_editable(other)
            && effective_keystroke_for(other, overrides, platform).as_deref() == Some(candidate)
    });
    match taken_by {
        Some(other) => Verdict::Conflict(other.id),
        None => Verdict::Accept,
    }
}

/// 新键位是不是 shell / TUI 的常用键:Ctrl+字母(不带 Shift,`^A`..`^Z` 是 readline 与
/// 各家 TUI 的命门)与几个 Ctrl+标点控制码,以及 Alt+单字符(readline 的 Meta 键:
/// Alt+B / F 按词移动、Alt+. 取上一条的参数)。是的话设置页就地提醒一句,但允许保存。
pub fn terminal_warning(keystroke: &str) -> bool {
    let Some(parsed) = parse_single(keystroke) else {
        return false;
    };
    let modifiers = parsed.modifiers;
    if modifiers.platform || modifiers.function {
        return false;
    }
    let key = parsed.key.as_str();
    let single_char = key.chars().count() == 1;
    if modifiers.control && !modifiers.shift && !modifiers.alt {
        return (single_char && key.chars().all(|c| c.is_ascii_alphabetic()))
            || matches!(key, "[" | "]" | "\\" | "/" | "@" | "^" | "_" | "space");
    }
    if modifiers.alt && !modifiers.control && !modifiers.shift {
        return single_char;
    }
    false
}

// ─── 往 gpui 绑 ──────────────────────────────────────────────

/// 把表与内部键位注册进 gpui。**启动时只调一次**(`main.rs`),之后的改键走
/// [`apply_overrides`] 追加。
///
/// gpui 的按键派发**先匹配 action 绑定、后跑 key 监听**,所以这里绑上就等于拿到了
/// 原版 capture 阶段 `consume(e)` 的效果:终端看不到这些键。
/// 构造一条动态键位绑定:运行期走 `KeyBinding::load`(零泄漏、使用真实键盘映射);
/// 无 `App` 的纯单测路径经 `make_binding_test` 走 `KeyBinding::new`。
fn load_binding<A: gpui::Action>(
    keystroke: &str,
    action: A,
    context: Option<&str>,
    cx: &App,
) -> KeyBinding {
    let predicate = context.map(|c| gpui::KeyBindingContextPredicate::parse(c).unwrap().into());
    KeyBinding::load(keystroke, Box::new(action), predicate, false, None, cx).unwrap()
}

#[cfg(test)]
fn test_binding<A: gpui::Action>(
    keystroke: &str,
    action: A,
    context: Option<&str>,
) -> KeyBinding {
    KeyBinding::new(
        Box::leak(keystroke.to_owned().into_boxed_str()),
        action,
        context,
    )
}

fn make_binding<A: gpui::Action>(
    keystroke: &str,
    action: A,
    context: Option<&str>,
    cx: Option<&App>,
) -> KeyBinding {
    match cx {
        Some(cx) => load_binding(keystroke, action, context, cx),
        #[cfg(test)]
        None => test_binding(keystroke, action, context),
        #[cfg(not(test))]
        None => unreachable!("runtime hotkey binding always has App"),
    }
}

pub fn bind_keys(cx: &mut App) {
    let mut bindings = table_bindings_with(Platform::current(), Some(cx));
    bindings.extend(internal_bindings());
    cx.bind_keys(bindings);
    APPLIED.with(|applied| *applied.borrow_mut() = Overrides::default());
}

/// 表里那几十条的默认绑定(含 `Ctrl+1..9` 展开的九条)。终端 scope 的不绑。
#[cfg(test)]
fn table_bindings(platform: Platform) -> Vec<KeyBinding> {
    table_bindings_with(platform, None)
}

fn table_bindings_with(platform: Platform, cx: Option<&App>) -> Vec<KeyBinding> {
    let mut bindings = Vec::new();
    for def in HOTKEYS {
        if def.scope == Scope::Terminal {
            continue;
        }
        if def.id == SELECT_PANE_N {
            // 选中叶内第 N 个 tab(表里是一条,这里展开成九条)
            for (n, keystroke) in digit_keystrokes(platform) {
                bindings.push(make_binding(&keystroke, SelectPane(n), Some(WORKSPACE), cx));
            }
            continue;
        }
        if let Some(binding) =
            binding_for_with(def.id, &default_keystroke_for(def, platform), cx)
        {
            bindings.push(binding);
        }
    }
    bindings
}

/// 让 keymap 跟上配置里的覆盖表:与上一次绑上去的那份比,变了的条目在旧键位上追加
/// `Unbind`、在新键位上追加绑定(机制见模块注释「用户改键」段)。**即时生效**,不用重启。
///
/// 启动时(`main.rs`,默认键位绑完之后)调一次,设置页每改一次键调一次
/// (`AppStore::set_hotkeys`)。返回读覆盖表时丢掉的条目说明,由调用方写日志。
pub fn apply_overrides(raw: &BTreeMap<String, String>, cx: &mut App) -> Vec<String> {
    let platform = Platform::current();
    let (next, issues) = Overrides::parse(raw, platform);
    let prev = applied_overrides();
    let bindings = rebind_bindings_with(&prev, &next, platform, Some(cx));
    if !bindings.is_empty() {
        cx.bind_keys(bindings);
    }
    APPLIED.with(|applied| *applied.borrow_mut() = next);
    issues
}

/// 从 `prev` 换到 `next` 要追加的绑定:先是所有旧键位上的 `Unbind(动作名)`,再是所有新键位
/// 上的动作绑定。先解后绑:两条互换键位时,后追加的绑定排在 `Unbind` 前面(同深度后注册的
/// 优先),不会被自己那批 `Unbind` 误伤 —— 何况 `Unbind` 只认同名动作。
#[cfg(test)]
fn rebind_bindings(prev: &Overrides, next: &Overrides, platform: Platform) -> Vec<KeyBinding> {
    rebind_bindings_with(prev, next, platform, None)
}

fn rebind_bindings_with(
    prev: &Overrides,
    next: &Overrides,
    platform: Platform,
    cx: Option<&App>,
) -> Vec<KeyBinding> {
    let mut unbinds = Vec::new();
    let mut binds = Vec::new();
    for def in HOTKEYS.iter().filter(|def| is_editable(def)) {
        let before = effective_keystroke_for(def, prev, platform);
        let after = effective_keystroke_for(def, next, platform);
        if before == after {
            continue;
        }
        if let Some(keystroke) = before
            && let Some(binding) = binding_for_with(def.id, &keystroke, cx)
        {
            unbinds.push(make_binding(
                &keystroke,
                Unbind(binding.action().name().into()),
                Some(context_of(def)),
                cx,
            ));
        }
        if let Some(keystroke) = after
            && let Some(binding) = binding_for_with(def.id, &keystroke, cx)
        {
            binds.push(binding);
        }
    }
    unbinds.extend(binds);
    unbinds
}

/// 这一条绑在哪个 context 上(`Unbind` 必须挂同一个,才与原绑定同深度、排在它前面)。
fn context_of(def: &HotkeyDef) -> &'static str {
    match def.scope {
        Scope::TerminalAction => TERMINAL,
        Scope::Global | Scope::Terminal => WORKSPACE,
    }
}

/// 不进表的内部键位:弹窗 / 面板 / 页签自己的键,以及解除组件库对终端 Tab 的抢键。
///
/// 单拎成一个函数有两个用处:[`bind_keys`] 把它们与表一起注册;设置页判冲突时
/// ([`judge`])拿它算「弹窗内部键位」—— 从真正绑上去的那份推,不另抄一张清单。
fn internal_bindings() -> Vec<KeyBinding> {
    // 项目切换器的方向键。**不进快捷键表** —— 它是弹窗内部键位,设置页不该列。
    //
    // 谓词写成 `"ProjectSwitcher > Input"` 而不是 `"ProjectSwitcher"`:只有与
    // `Input` **同深度**才压得过组件库自带的 `up`/`down`(单行输入框那两个处理器
    // 直接 return、不 propagate,挂在容器上的 on_key_down 永远收不到)。
    // 详见 project_switcher 模块注释,机制有单测钉住。
    let mut bindings: Vec<KeyBinding> = vec![
        KeyBinding::new(
            "up",
            project_switcher::SwitcherPrev,
            Some("ProjectSwitcher > Input"),
        ),
        KeyBinding::new(
            "down",
            project_switcher::SwitcherNext,
            Some("ProjectSwitcher > Input"),
        ),
        // 翻页与首尾(三个搜索类浮层同一组键)。裸 home / end 不占:那是输入框里挪光标的键
        KeyBinding::new(
            "pageup",
            project_switcher::SwitcherPageUp,
            Some("ProjectSwitcher > Input"),
        ),
        KeyBinding::new(
            "pagedown",
            project_switcher::SwitcherPageDown,
            Some("ProjectSwitcher > Input"),
        ),
        KeyBinding::new(
            "ctrl-home",
            project_switcher::SwitcherFirst,
            Some("ProjectSwitcher > Input"),
        ),
        KeyBinding::new(
            "ctrl-end",
            project_switcher::SwitcherLast,
            Some("ProjectSwitcher > Input"),
        ),
    ];

    // 快速打开的方向键。与项目切换器同一套理由:弹窗内部键位不进表,谓词要与 `Input`
    // 同深度才压得过它自带的 up/down(机制见 project_switcher 模块注释)
    let quick_open_input = format!("{} > Input", quick_open::CONTEXT);
    let quick_open_input = Some(quick_open_input.as_str());
    bindings.push(KeyBinding::new(
        "up",
        quick_open::QuickOpenPrev,
        quick_open_input,
    ));
    bindings.push(KeyBinding::new(
        "down",
        quick_open::QuickOpenNext,
        quick_open_input,
    ));
    bindings.push(KeyBinding::new(
        "pageup",
        quick_open::QuickOpenPageUp,
        quick_open_input,
    ));
    bindings.push(KeyBinding::new(
        "pagedown",
        quick_open::QuickOpenPageDown,
        quick_open_input,
    ));
    bindings.push(KeyBinding::new(
        "ctrl-home",
        quick_open::QuickOpenFirst,
        quick_open_input,
    ));
    bindings.push(KeyBinding::new(
        "ctrl-end",
        quick_open::QuickOpenLast,
        quick_open_input,
    ));

    // 全局搜索的结果游标:↑/↓ 逐条、PageUp/PageDown 翻页、Ctrl/Cmd+Enter 打开但留着浮层。
    // 同一套理由:弹窗内部键位不进表,谓词与 `Input` 同深度才压得过它自带的那几条
    // (输入框把 pageup/pagedown 绑成了单行里什么都不做、也不 propagate 的 MovePage*)
    let search_input = format!("{} > Input", search_modal::CONTEXT);
    let search_input = Some(search_input.as_str());
    bindings.push(KeyBinding::new(
        "up",
        search_modal::SearchCursorPrev,
        search_input,
    ));
    bindings.push(KeyBinding::new(
        "down",
        search_modal::SearchCursorNext,
        search_input,
    ));
    bindings.push(KeyBinding::new(
        "pageup",
        search_modal::SearchCursorPageUp,
        search_input,
    ));
    bindings.push(KeyBinding::new(
        "pagedown",
        search_modal::SearchCursorPageDown,
        search_input,
    ));
    bindings.push(KeyBinding::new(
        "ctrl-home",
        search_modal::SearchCursorFirst,
        search_input,
    ));
    bindings.push(KeyBinding::new(
        "ctrl-end",
        search_modal::SearchCursorLast,
        search_input,
    ));
    for keystroke in ["ctrl-enter", "cmd-enter"] {
        bindings.push(KeyBinding::new(
            keystroke,
            search_modal::SearchOpenKeep,
            search_input,
        ));
    }

    // 命令库浮层的方向键 / Ctrl+↵(issue #81)。与项目切换器同一套理由:
    // 弹窗内部键位不进表,谓词要与 `Input` 同深度才压得过它自带的 up/down。
    bindings.push(KeyBinding::new(
        "up",
        command_library::popover::CommandLibraryPrev,
        Some("CommandLibrary > Input"),
    ));
    bindings.push(KeyBinding::new(
        "down",
        command_library::popover::CommandLibraryNext,
        Some("CommandLibrary > Input"),
    ));
    bindings.push(KeyBinding::new(
        "ctrl-enter",
        command_library::popover::CommandLibraryPasteOnly,
        Some("CommandLibrary > Input"),
    ));
    bindings.push(KeyBinding::new(
        "cmd-enter",
        command_library::popover::CommandLibraryPasteOnly,
        Some("CommandLibrary > Input"),
    ));

    // 终端右键「SSH 连接」快速连接面板的方向键。与项目切换器同一套理由:
    // 面板内部键位不进表,谓词要与 `Input` 同深度才压得过它自带的 up/down。
    bindings.push(KeyBinding::new(
        "up",
        crate::ssh_quick_connect::SshQuickPrev,
        Some("SshQuickConnect > Input"),
    ));
    bindings.push(KeyBinding::new(
        "down",
        crate::ssh_quick_connect::SshQuickNext,
        Some("SshQuickConnect > Input"),
    ));
    // Ctrl+↵(macOS 另认 Cmd+↵):在新终端里连。与命令库的 Ctrl+↵ 同一写法,两个键各平台都绑
    for keystroke in ["ctrl-enter", "cmd-enter"] {
        bindings.push(KeyBinding::new(
            keystroke,
            crate::ssh_quick_connect::SshQuickConnectInNewTerminal,
            Some("SshQuickConnect > Input"),
        ));
    }

    // Git 分支弹窗:↑↓ 移动高亮、→ 弹出高亮分支的操作菜单。与项目切换器同一套理由
    // (谓词与 `Input` 同深度才压得过它自带的 up/down/right);→ 只在光标位于搜索词
    // 末尾时才接管,否则处理器 `propagate()` 让给输入框的 MoveRight(见 git_branches)。
    let branch_popup = format!("{} > Input", crate::git::branches::BRANCH_POPUP_CONTEXT);
    let branch_popup = Some(branch_popup.as_str());
    bindings.push(KeyBinding::new(
        "up",
        crate::git::branches::BranchPopupPrev,
        branch_popup,
    ));
    bindings.push(KeyBinding::new(
        "down",
        crate::git::branches::BranchPopupNext,
        branch_popup,
    ));
    bindings.push(KeyBinding::new(
        "right",
        crate::git::branches::BranchPopupMenu,
        branch_popup,
    ));

    // Git 提交框的 Ctrl/Cmd+Enter(`GitChanges.tsx:411-415`)。**不进快捷键表** ——
    // 原版它是 textarea 的 onKeyDown,不在 hotkeys.ts 里。谓词同上要与 `Input` 同深度。
    let commit_box = format!("{} > Input", git_changes::CHANGES_CONTEXT);
    let commit_box = Some(commit_box.as_str());
    bindings.push(KeyBinding::new(
        "ctrl-enter",
        git_changes::GitCommitMessage,
        commit_box,
    ));
    bindings.push(KeyBinding::new(
        "cmd-enter",
        git_changes::GitCommitMessage,
        commit_box,
    ));
    // 提交并推送(Ctrl/Cmd+Alt+Enter),同一个谓词:提交成功后打开推送对话框(预选当前
    // 分支),确认了才推 —— 与 IDEA 的 Commit and Push 一致。
    bindings.push(KeyBinding::new(
        "ctrl-alt-enter",
        git_changes::GitCommitAndPush,
        commit_box,
    ));
    bindings.push(KeyBinding::new(
        "cmd-alt-enter",
        git_changes::GitCommitAndPush,
        commit_box,
    ));

    // 更改列表的键盘操作(↑/↓ 选择、Space 暂存切换、Enter 看 diff、Delete 丢弃)。
    // 只在列表自己持有焦点时生效(点过列表行才会),终端 / 输入框里按这些键照旧归它们 ——
    // 谓词是列表自己的 context,与 `Terminal` / `Input` 都不在一条派发链上。
    let list = Some(git_changes::LIST_CONTEXT);
    bindings.push(KeyBinding::new(
        "up",
        git_changes::GitChangesSelectPrev,
        list,
    ));
    bindings.push(KeyBinding::new(
        "down",
        git_changes::GitChangesSelectNext,
        list,
    ));
    bindings.push(KeyBinding::new(
        "space",
        git_changes::GitChangesToggleStage,
        list,
    ));
    bindings.push(KeyBinding::new(
        "enter",
        git_changes::GitChangesOpenDiff,
        list,
    ));
    bindings.push(KeyBinding::new(
        "delete",
        git_changes::GitChangesDiscard,
        list,
    ));

    // 单文件 diff 页签(磁盘对比页签同一个 context):F7 / Shift+F7 跳改动,Alt+← / Alt+→
    // 换文件(IDEA 的键位)。谓词是页签正文自己的 context,比 `Workspace` 深,焦点在页签里时
    // 先试这一条;全局的 Alt+方向键(切 pane 焦点)只在终端页生效,页签页上本来就不动。
    bindings.push(KeyBinding::new(
        "f7",
        git_diff::GitDiffNextChange,
        Some(git_diff::FILE_DIFF_CONTEXT),
    ));
    bindings.push(KeyBinding::new(
        "shift-f7",
        git_diff::GitDiffPrevChange,
        Some(git_diff::FILE_DIFF_CONTEXT),
    ));
    bindings.push(KeyBinding::new(
        "alt-right",
        git_diff::GitDiffNextFile,
        Some(git_diff::FILE_DIFF_CONTEXT),
    ));
    bindings.push(KeyBinding::new(
        "alt-left",
        git_diff::GitDiffPrevFile,
        Some(git_diff::FILE_DIFF_CONTEXT),
    ));
    // Esc 先清行勾选(按行暂存);没有勾选时处理器 `propagate`,按键照常往外走(页签里
    // 没有弹窗可关,焦点也不动)
    bindings.push(KeyBinding::new(
        "escape",
        git_diff::GitDiffClearPicks,
        Some(git_diff::FILE_DIFF_CONTEXT),
    ));
    // 冲突文件「逐段解决」弹窗:F7 / Shift+F7 跳下一段 / 上一段(与上面跳改动同一对键)。
    // 谓词同样是弹窗自己的 context;两侧原文框是只读的 Textarea,不绑 F7,按键照常冒上来
    bindings.push(KeyBinding::new(
        "f7",
        git_conflict::GitConflictNextSegment,
        Some(git_conflict::SEGMENTS_CONTEXT),
    ));
    bindings.push(KeyBinding::new(
        "shift-f7",
        git_conflict::GitConflictPrevSegment,
        Some(git_conflict::SEGMENTS_CONTEXT),
    ));
    // 提交 diff / 合并变更页签:↑ / ↓ 换左栏的上一个 / 下一个文件(与单文件历史的 ↑ / ↓ 换
    // 提交同一手感)。谓词是页签焦点容器自己的 context,正文没有方向键绑定,不抢它的拖选 / 滚动
    bindings.push(KeyBinding::new(
        "down",
        git_diff::GitCommitDiffNextFile,
        Some(git_diff::COMMIT_DIFF_CONTEXT),
    ));
    bindings.push(KeyBinding::new(
        "up",
        git_diff::GitCommitDiffPrevFile,
        Some(git_diff::COMMIT_DIFF_CONTEXT),
    ));

    // Git 提交历史(焦点在历史区时)。**不进快捷键表** —— 面板内部键位,同上。
    // `GitHistory` 比 `Workspace` 深,Ctrl+F 在历史区里归过滤框、出了历史区照旧是
    // 终端查找;过滤框自己的 ↑/↓/Enter/Ctrl+C 绑在更深的 `Input` 上,先轮到它。
    // Esc 要与 `Input` 同深度才压得过输入框自带的那条(理由同项目切换器)。
    let history = Some(git_history::HISTORY_CONTEXT);
    bindings.push(KeyBinding::new(
        "up",
        git_history::GitLogSelectPrev,
        history,
    ));
    bindings.push(KeyBinding::new(
        "down",
        git_history::GitLogSelectNext,
        history,
    ));
    // Shift+↑/↓ 从锚点扩展 / 收缩多选(过滤框里的 Shift+↑/↓ 归更深的 `Input`,照旧选文字)
    bindings.push(KeyBinding::new(
        "shift-up",
        git_history::GitLogExtendPrev,
        history,
    ));
    bindings.push(KeyBinding::new(
        "shift-down",
        git_history::GitLogExtendNext,
        history,
    ));
    bindings.push(KeyBinding::new(
        "enter",
        git_history::GitLogOpenSelected,
        history,
    ));
    for keystroke in ["ctrl-c", "cmd-c"] {
        bindings.push(KeyBinding::new(
            keystroke,
            git_history::GitLogCopyHash,
            history,
        ));
    }
    for keystroke in ["ctrl-f", "cmd-f"] {
        bindings.push(KeyBinding::new(
            keystroke,
            git_history::GitLogFocusFilter,
            history,
        ));
    }
    // Esc:列表上关提交详情栏;过滤框里那一条(更深的谓词,先试)清空过滤,不在过滤框时
    // 它放行,落到这一条
    bindings.push(KeyBinding::new(
        "escape",
        git_history::GitLogCloseDetail,
        history,
    ));
    let history_input = format!("{} > Input", git_history::HISTORY_CONTEXT);
    bindings.push(KeyBinding::new(
        "escape",
        git_history::GitLogClearFilter,
        Some(history_input.as_str()),
    ));
    // 「修改提交消息」多行框里回车是换行,Ctrl/Cmd+Enter 才是确认
    let reword_input = format!("{} > Input", git_history::REWORD_CONTEXT);
    for keystroke in ["ctrl-enter", "cmd-enter"] {
        bindings.push(KeyBinding::new(
            keystroke,
            git_history::GitLogRewordConfirm,
            Some(reword_input.as_str()),
        ));
    }
    // 单文件历史页签:↑/↓ 换提交
    bindings.push(KeyBinding::new(
        "up",
        git_file_history::FileHistoryPrev,
        Some(git_file_history::FILE_HISTORY_CONTEXT),
    ));
    bindings.push(KeyBinding::new(
        "down",
        git_file_history::FileHistoryNext,
        Some(git_file_history::FILE_HISTORY_CONTEXT),
    ));
    // 三种 diff 页签(单文件 / 提交 / 单文件历史)的正文选中复制:Ctrl/Cmd+C 复制选区、
    // Ctrl/Cmd+A 全选当前栏。谓词是各页签焦点容器自己的 context;没有可复制 / 可选的
    // 内容时处理器 `propagate`,按键照常往外走。F7 / Alt+←→ / ↑↓ / Esc 不受影响
    for context in [
        git_diff::FILE_DIFF_CONTEXT,
        git_diff::COMMIT_DIFF_CONTEXT,
        git_file_history::FILE_HISTORY_CONTEXT,
    ] {
        for keystroke in ["ctrl-c", "cmd-c"] {
            bindings.push(KeyBinding::new(
                keystroke,
                git_diff::GitDiffCopy,
                Some(context),
            ));
        }
        for keystroke in ["ctrl-a", "cmd-a"] {
            bindings.push(KeyBinding::new(
                keystroke,
                git_diff::GitDiffSelectAll,
                Some(context),
            ));
        }
    }
    // 贮藏列表弹窗:↑/↓ 换贮藏,Delete 删除(先确认);Enter 应用走 Dialog 自己的 Confirm
    bindings.push(KeyBinding::new(
        "up",
        git_stash::GitStashPrev,
        Some(git_stash::STASH_CONTEXT),
    ));
    bindings.push(KeyBinding::new(
        "down",
        git_stash::GitStashNext,
        Some(git_stash::STASH_CONTEXT),
    ));
    bindings.push(KeyBinding::new(
        "delete",
        git_stash::GitStashDrop,
        Some(git_stash::STASH_CONTEXT),
    ));

    // 文件查看器源码态的「上一处 / 下一处 Git 变更」(IDEA 键位 Ctrl+Alt+Shift+↑/↓)。
    // **不进快捷键表**:只在文件页里有意义,谓词挂 `FileViewer`;与编辑器自带的
    // `ctrl-alt-up/down`(加光标)修饰键不同,不冲突。键位与机制单测见 `file_viewer::git_gutter`
    bindings.extend(crate::file_viewer::git_change_key_bindings());

    // 终端里的裸 Tab / Shift+Tab 必须归终端(shell 补全、Claude 的 Tab 切模式全靠它)。
    //
    // 组件库的 `Root` 在 `root::init` 里把这两个键绑成了 `focus_next` / `focus_prev`,
    // 而按上面那条铁律「先匹配 action 绑定、后跑 key 监听」—— 于是在终端里按 Tab
    // 只会把焦点挪到下一个可聚焦元素,`TerminalView::on_key_down` 一个字节都收不到,
    // 表现就是「Tab 没反应,而且之后要重新点一下终端才能继续打字」。
    //
    // 解法是在**更深**的 `Terminal` context 上用 `NoAction` 把它们压掉:
    // `Keymap::bindings_for_input` 先按 context 深度排序,遇到 `NoAction` 直接 break,
    // 更浅的 `Root` 那两条不再参与,这次按键退回 key 监听路径,由终端自己翻成
    // `\t` / `ESC [ Z`。深度优先的用法与上面 `ProjectSwitcher > Input` 同源。
    //
    // **不进快捷键表**:它不是一条「应用快捷键」,而是解除组件库对终端的抢键,
    // 设置页列出来只会让人以为 Tab 是个可改键位的功能。
    bindings.push(KeyBinding::new("tab", NoAction, Some(TERMINAL)));
    bindings.push(KeyBinding::new("shift-tab", NoAction, Some(TERMINAL)));

    bindings
}

/// id → action 的分派。**新增表项时这里也要加一条**,否则那条键位静默绑不上
/// (设置页照样列出来 = 又一次「写着能按、按了没反应」)。
///
/// 返回 `None` 只应发生在终端 scope 与 `selectPaneN` 上(前者不绑,后者由
/// [`table_bindings`] 单独展开)—— 有 [`tests::全局条目都绑得上_action`] 盯着。
#[cfg(test)]
fn binding_for(id: &str, keystroke: &str) -> Option<KeyBinding> {
    binding_for_with(id, keystroke, None)
}

fn binding_for_with(id: &str, keystroke: &str, cx: Option<&App>) -> Option<KeyBinding> {
    Some(match id {
        "newTerminal" => make_binding(keystroke, NewTerminal, Some(WORKSPACE), cx),
        "closePane" => make_binding(keystroke, ClosePane, Some(WORKSPACE), cx),
        "closeTab" => make_binding(keystroke, CloseTerminalTab, Some(WORKSPACE), cx),
        "renamePane" => make_binding(keystroke, RenamePane, Some(WORKSPACE), cx),
        "splitRight" => make_binding(keystroke, SplitRight, Some(WORKSPACE), cx),
        "splitDown" => make_binding(keystroke, SplitDown, Some(WORKSPACE), cx),
        "toggleMaximize" => make_binding(keystroke, ToggleMaximizePane, Some(WORKSPACE), cx),
        "reopenClosed" => make_binding(keystroke, ReopenClosedTerminal, Some(WORKSPACE), cx),
        "nextPane" => make_binding(keystroke, NextPane, Some(WORKSPACE), cx),
        "prevPane" => make_binding(keystroke, PrevPane, Some(WORKSPACE), cx),
        "focusLeft" => make_binding(keystroke, FocusLeft, Some(WORKSPACE), cx),
        "focusRight" => make_binding(keystroke, FocusRight, Some(WORKSPACE), cx),
        "focusUp" => make_binding(keystroke, FocusUp, Some(WORKSPACE), cx),
        "focusDown" => make_binding(keystroke, FocusDown, Some(WORKSPACE), cx),
        "toggleWorkbenchPage" => make_binding(keystroke, ToggleWorkbenchPage, Some(WORKSPACE), cx),
        "moveTabLeft" => make_binding(keystroke, MoveTabLeft, Some(WORKSPACE), cx),
        "moveTabRight" => make_binding(keystroke, MoveTabRight, Some(WORKSPACE), cx),
        "resizeLeft" => make_binding(keystroke, ResizeSplitLeft, Some(WORKSPACE), cx),
        "resizeRight" => make_binding(keystroke, ResizeSplitRight, Some(WORKSPACE), cx),
        "resizeUp" => make_binding(keystroke, ResizeSplitUp, Some(WORKSPACE), cx),
        "resizeDown" => make_binding(keystroke, ResizeSplitDown, Some(WORKSPACE), cx),
        "switchProject" => make_binding(keystroke, SwitchProject, Some(WORKSPACE), cx),
        "globalSearch" => make_binding(keystroke, GlobalSearch, Some(WORKSPACE), cx),
        "quickOpen" => make_binding(keystroke, QuickOpenFile, Some(WORKSPACE), cx),
        "terminalSearch" => make_binding(keystroke, TerminalSearch, Some(WORKSPACE), cx),
        "openSettings" => make_binding(keystroke, OpenTerminalSettings, Some(WORKSPACE), cx),
        "toggleSidebar" => make_binding(keystroke, ToggleMiddleColumn, Some(WORKSPACE), cx),
        "toggleSessions" => make_binding(keystroke, ToggleSessions, Some(WORKSPACE), cx),
        "toggleUsage" => make_binding(keystroke, ToggleUsage, Some(WORKSPACE), cx),
        "jumpAttention" => make_binding(keystroke, JumpAttention, Some(WORKSPACE), cx),
        "commandLibrary" => make_binding(keystroke, ToggleCommandLibrary, Some(WORKSPACE), cx),
        "markerPrev" => make_binding(keystroke, MarkerPrev, Some(WORKSPACE), cx),
        "markerNext" => make_binding(keystroke, MarkerNext, Some(WORKSPACE), cx),
        // 挂 `Terminal`:只在终端有焦点时命中(见 [`Scope::TerminalAction`])
        "zoomIn" => make_binding(keystroke, TerminalZoomIn, Some(TERMINAL), cx),
        "zoomOut" => make_binding(keystroke, TerminalZoomOut, Some(TERMINAL), cx),
        "zoomReset" => make_binding(keystroke, TerminalZoomReset, Some(TERMINAL), cx),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Action, KeyContext, Keymap};

    fn ks(keystroke: &str) -> Keystroke {
        Keystroke::parse_with_os(keystroke, os_of(Platform::current()))
            .unwrap_or_else(|e| panic!("{keystroke} 解析失败: {e}"))
    }

    fn contexts(names: &[&str]) -> Vec<KeyContext> {
        names
            .iter()
            .map(|n| KeyContext::parse(n).unwrap())
            .collect()
    }

    /// 焦点在终端上时的 context 栈(外层 Root → Workspace → 终端本体)。
    fn in_terminal() -> Vec<KeyContext> {
        contexts(&["Root", WORKSPACE, TERMINAL])
    }

    fn overrides_on(pairs: &[(&str, &str)], platform: Platform) -> Overrides {
        let raw: BTreeMap<String, String> = pairs
            .iter()
            .map(|(id, keystroke)| (id.to_string(), keystroke.to_string()))
            .collect();
        let (overrides, issues) = Overrides::parse(&raw, platform);
        assert!(issues.is_empty(), "{issues:?}");
        overrides
    }

    fn overrides(pairs: &[(&str, &str)]) -> Overrides {
        overrides_on(pairs, Platform::Other)
    }

    /// 某个键在这个 context 栈上命中的动作名(按优先级)。
    fn hits(keymap: &Keymap, keystroke: &str, stack: &[KeyContext]) -> Vec<&'static str> {
        let (bindings, pending) = keymap.bindings_for_input(&[ks(keystroke)], stack);
        assert!(!pending, "{keystroke} 不该挂起等后续按键");
        bindings.iter().map(|b| b.action().name()).collect()
    }

    // 组件库 `Root` 那两条绑定的替身:它的 action 类型(`gpui_component::root::Tab`)
    // 没有 pub 出来,这里只需要「同一个键位、更浅的 context、某个会被消费的 action」。
    gpui::actions!(root_stub, [RootTab, RootTabPrev]);

    /// 复刻真实注册顺序:组件库 `Root` 的绑定在前,`bind_keys` 的表与内部键位在后。
    fn full_keymap(platform: Platform) -> Keymap {
        let mut keymap = Keymap::new(vec![
            KeyBinding::new("tab", RootTab, Some("Root")),
            KeyBinding::new("shift-tab", RootTabPrev, Some("Root")),
        ]);
        keymap.add_bindings(table_bindings(platform));
        keymap.add_bindings(internal_bindings());
        keymap
    }

    /// 显示串必须从**真正绑上去的**键位推出来 —— 原版注释里「改了键位忘改说明就漂移」
    /// 的防线(此前是「另存的 `combo` 与键位串逐字段对照」,现在显示串不另存了,
    /// 改成反过来:拿进了 keymap 的那条 binding 反推显示串,与 [`label_for`] 对照)。
    /// 两个平台、默认与改过(含解绑)之后各走一遍。
    #[test]
    fn 显示串从实际绑定推导() {
        for platform in [Platform::Other, Platform::Mac] {
            let custom = overrides_on(
                &[
                    ("newTerminal", "ctrl-alt-n"),
                    ("zoomIn", "ctrl-alt-="),
                    ("terminalSearch", ""),
                ],
                platform,
            );
            for overrides in [Overrides::default(), custom] {
                let mut keymap = Keymap::new(table_bindings(platform));
                keymap.add_bindings(rebind_bindings(&Overrides::default(), &overrides, platform));
                for def in HOTKEYS.iter().filter(|def| is_editable(def)) {
                    let probe = binding_for(def.id, &default_keystroke_for(def, platform)).unwrap();
                    // 同一个动作绑过多次时,`bindings_for_action` 已滤掉被 `Unbind` 压住的,
                    // 最后一条就是此刻生效的那条
                    let bound = keymap.bindings_for_action(probe.action()).last().map(|b| {
                        let keystroke = b.keystrokes()[0].inner();
                        modifiers_label(&keystroke.modifiers, &key_label(&keystroke.key), platform)
                    });
                    assert_eq!(
                        bound,
                        label_for(def, &overrides, platform),
                        "{} 在 {platform:?} 上显示与实际绑定对不上",
                        def.id
                    );
                }
            }
        }
    }

    /// 可改的条目都能分派到 action —— 分派表漏一条 = 设置页列着但按不出效果;
    /// 终端 scope 绝不进 keymap(绑了就轮不到终端自己消费 Ctrl+Shift+C/V)。
    #[test]
    fn 全局条目都绑得上_action() {
        for def in HOTKEYS {
            let keystroke = default_keystroke_for(def, Platform::Other);
            match def.scope {
                // 唯一不走 `binding_for` 的全局条目是 Ctrl+1..9(table_bindings 单独展开)
                Scope::Global if def.id == SELECT_PANE_N => {
                    assert!(binding_for(def.id, &keystroke).is_none());
                }
                Scope::Global | Scope::TerminalAction => {
                    assert!(
                        binding_for(def.id, &keystroke).is_some(),
                        "{} 没有 action",
                        def.id
                    )
                }
                Scope::Terminal => {
                    assert!(
                        binding_for(def.id, &keystroke).is_none(),
                        "{} 不该绑键位",
                        def.id
                    )
                }
            }
        }
        for platform in [Platform::Other, Platform::Mac] {
            let bound: Vec<String> = table_bindings(platform)
                .iter()
                .map(|b| {
                    let keystroke = b.keystrokes()[0].inner();
                    canonical(&keystroke.modifiers, &keystroke.key)
                })
                .collect();
            for def in HOTKEYS.iter().filter(|def| def.scope == Scope::Terminal) {
                let keystroke = default_keystroke_for(def, platform);
                assert!(!bound.contains(&keystroke), "{} 不许绑 action", def.id);
            }
        }
    }

    /// **Windows / Linux 上绑进 keymap 的默认键位与这次改造前逐条一致**:下面这张是改造前
    /// `bind_keys` 表段的产出照抄(键位 + 动作 + context,含 Ctrl+1..9)。mac 的换算、
    /// 显示串的改造都不许漏到这边来。
    #[test]
    fn windows_linux_默认绑定与改造前逐条一致() {
        let mut expected = vec![
            KeyBinding::new("ctrl-shift-t", NewTerminal, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-w", ClosePane, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-x", CloseTerminalTab, Some(WORKSPACE)),
            KeyBinding::new("f2", RenamePane, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-d", SplitRight, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-e", SplitDown, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-m", ToggleMaximizePane, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-z", ReopenClosedTerminal, Some(WORKSPACE)),
            KeyBinding::new("ctrl-tab", NextPane, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-tab", PrevPane, Some(WORKSPACE)),
            KeyBinding::new("alt-left", FocusLeft, Some(WORKSPACE)),
            KeyBinding::new("alt-right", FocusRight, Some(WORKSPACE)),
            KeyBinding::new("alt-up", FocusUp, Some(WORKSPACE)),
            KeyBinding::new("alt-down", FocusDown, Some(WORKSPACE)),
            KeyBinding::new("ctrl-`", ToggleWorkbenchPage, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-pageup", MoveTabLeft, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-pagedown", MoveTabRight, Some(WORKSPACE)),
            KeyBinding::new("alt-shift-left", ResizeSplitLeft, Some(WORKSPACE)),
            KeyBinding::new("alt-shift-right", ResizeSplitRight, Some(WORKSPACE)),
            KeyBinding::new("alt-shift-up", ResizeSplitUp, Some(WORKSPACE)),
            KeyBinding::new("alt-shift-down", ResizeSplitDown, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-p", SwitchProject, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-f", GlobalSearch, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-o", QuickOpenFile, Some(WORKSPACE)),
            KeyBinding::new("ctrl-f", TerminalSearch, Some(WORKSPACE)),
            KeyBinding::new("ctrl-,", OpenTerminalSettings, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-b", ToggleMiddleColumn, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-a", ToggleSessions, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-u", ToggleUsage, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-j", JumpAttention, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-k", ToggleCommandLibrary, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-up", MarkerPrev, Some(WORKSPACE)),
            KeyBinding::new("ctrl-shift-down", MarkerNext, Some(WORKSPACE)),
            KeyBinding::new("ctrl-=", TerminalZoomIn, Some(TERMINAL)),
            KeyBinding::new("ctrl--", TerminalZoomOut, Some(TERMINAL)),
            KeyBinding::new("ctrl-0", TerminalZoomReset, Some(TERMINAL)),
        ];
        for n in 1..=9usize {
            expected.push(test_binding(
                &format!("ctrl-{n}"),
                SelectPane(n),
                Some(WORKSPACE),
            ));
        }
        let actual = table_bindings(Platform::Other);
        assert_eq!(actual.len(), expected.len());
        for want in &expected {
            let found = actual
                .iter()
                .filter(|got| {
                    got.keystrokes() == want.keystrokes()
                        && got.action().partial_eq(want.action())
                        && got.predicate() == want.predicate()
                })
                .count();
            assert_eq!(
                found,
                1,
                "{} @ {:?} 与改造前不一致",
                want.action().name(),
                want.keystrokes()[0].inner().unparse(os_of(Platform::Other))
            );
        }
    }

    /// **macOS 的默认键位**逐条钉住:应用级动作的主修饰键换成 ⌘;Ctrl+Tab / Ctrl+Shift+Tab /
    /// Ctrl+` 与回看两条保持 Control;纯 Alt 的不变。
    #[test]
    fn mac_默认键位按表换算() {
        let expected = [
            ("newTerminal", "cmd-shift-t"),
            ("closePane", "cmd-shift-w"),
            ("closeTab", "cmd-shift-x"),
            ("renamePane", "f2"),
            ("splitRight", "cmd-shift-d"),
            ("splitDown", "cmd-shift-e"),
            ("toggleMaximize", "cmd-shift-m"),
            ("reopenClosed", "cmd-shift-z"),
            ("nextPane", "ctrl-tab"),
            ("prevPane", "ctrl-shift-tab"),
            ("selectPaneN", "cmd-1"),
            ("focusLeft", "alt-left"),
            ("focusRight", "alt-right"),
            ("focusUp", "alt-up"),
            ("focusDown", "alt-down"),
            ("toggleWorkbenchPage", "ctrl-`"),
            ("moveTabLeft", "cmd-shift-pageup"),
            ("moveTabRight", "cmd-shift-pagedown"),
            ("resizeLeft", "alt-shift-left"),
            ("resizeRight", "alt-shift-right"),
            ("resizeUp", "alt-shift-up"),
            ("resizeDown", "alt-shift-down"),
            ("switchProject", "cmd-shift-p"),
            ("globalSearch", "cmd-shift-f"),
            ("quickOpen", "cmd-shift-o"),
            ("terminalSearch", "cmd-f"),
            ("openSettings", "cmd-,"),
            ("toggleSidebar", "cmd-shift-b"),
            ("toggleSessions", "cmd-shift-a"),
            ("toggleUsage", "cmd-shift-u"),
            ("jumpAttention", "cmd-shift-j"),
            ("commandLibrary", "cmd-shift-k"),
            ("markerPrev", "cmd-shift-up"),
            ("markerNext", "cmd-shift-down"),
            ("copySelection", "cmd-shift-c"),
            ("pasteToTerminal", "cmd-shift-v"),
            ("scrollPageUp", "shift-pageup"),
            ("scrollPageDown", "shift-pagedown"),
            ("scrollTop", "ctrl-shift-home"),
            ("scrollBottom", "ctrl-shift-end"),
            ("zoomIn", "cmd-="),
            ("zoomOut", "cmd--"),
            ("zoomReset", "cmd-0"),
        ];
        assert_eq!(expected.len(), HOTKEYS.len(), "表里每一条都要列在这里");
        for (id, keystroke) in expected {
            let def = find(id).unwrap();
            assert_eq!(default_keystroke_for(def, Platform::Mac), keystroke, "{id}");
        }
        let digits: Vec<String> = digit_keystrokes(Platform::Mac)
            .into_iter()
            .map(|(_, keystroke)| keystroke)
            .collect();
        assert_eq!(digits.first().map(String::as_str), Some("cmd-1"));
        assert_eq!(digits.last().map(String::as_str), Some("cmd-9"));
        // Windows / Linux 上表里写的就是默认键位
        for def in HOTKEYS {
            assert_eq!(
                default_keystroke_for(def, Platform::Other),
                def.keystroke,
                "{}",
                def.id
            );
        }
    }

    /// 两个平台的默认键位都合法、不撞系统键、不抢终端自己消费的键(mac 上 ⌘C / ⌘V /
    /// ⌘⇧C / ⌘⇧V / ⌘← / ⌘→ 都归 `TerminalView`)。
    #[test]
    fn 默认键位不撞系统键也不抢终端的键() {
        for platform in [Platform::Other, Platform::Mac] {
            for def in HOTKEYS.iter().filter(|def| is_editable(def)) {
                let keystroke = default_keystroke_for(def, platform);
                let parsed = ks(&keystroke);
                assert_eq!(
                    combo_problem(&parsed.modifiers, &parsed.key, None, platform),
                    None,
                    "{} 在 {platform:?} 上不合法",
                    def.id
                );
                assert_eq!(
                    terminal_owner(&keystroke, platform, true),
                    None,
                    "{} 在 {platform:?} 上抢了终端的键",
                    def.id
                );
            }
        }
    }

    /// 表里的键位串、保留清单、内部键位清单都是规范形 —— 判冲突直接比字符串,
    /// 写法不一致就会「明明是同一个键却说不冲突」。
    #[test]
    fn 键位串都是规范形() {
        let mut all: Vec<&str> = HOTKEYS.iter().map(|def| def.keystroke).collect();
        all.extend(RESERVED_OTHER);
        all.extend(RESERVED_MAC);
        all.extend(SMART_COPY_PASTE.iter().map(|(_, keystroke)| *keystroke));
        for platform in [Platform::Other, Platform::Mac] {
            all.extend(listener_keys(platform));
            all.extend(component_keys(platform));
        }
        for keystroke in all {
            assert_eq!(normalize_keystroke(keystroke).as_deref(), Some(keystroke));
        }
        // 写法不同的同一个键规范成同一串
        assert_eq!(
            normalize_keystroke("Shift-Ctrl-T").as_deref(),
            Some("ctrl-shift-t")
        );
        assert_eq!(
            normalize_keystroke("super-shift-k").as_deref(),
            Some("cmd-shift-k")
        );
        assert_eq!(normalize_keystroke("ctrl-k ctrl-s"), None, "多键序列不收");
        assert_eq!(normalize_keystroke(""), None);
    }

    /// 同一平台上绑进 keymap 的键位串不重复 —— 两条撞同一个键,后注册的那条静默吃掉前一条。
    #[test]
    fn 全局键位串不重复() {
        for platform in [Platform::Other, Platform::Mac] {
            let mut keystrokes: Vec<String> = table_bindings(platform)
                .iter()
                .map(|b| {
                    let keystroke = b.keystrokes()[0].inner();
                    canonical(&keystroke.modifiers, &keystroke.key)
                })
                .collect();
            keystrokes.sort_unstable();
            let count = keystrokes.len();
            keystrokes.dedup();
            assert_eq!(keystrokes.len(), count, "{platform:?}");
        }
    }

    /// id 不重复 —— 设置页拿 id 当行 key,覆盖表拿 id 当键。
    #[test]
    fn id_不重复() {
        let mut ids: Vec<&str> = HOTKEYS.iter().map(|d| d.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
    }

    /// 分组保持声明顺序、组内保持表内顺序(设置页照这个顺序画)。
    ///
    /// 组序与原版 `hotkeys.ts:42-46` 一致:终端 / 导航 / 全局 / AI 任务标记 / 剪贴板;
    /// GPUI 侧补的「页签与分屏」紧跟导航,「回看与缩放」排在最后。
    #[test]
    fn 分组保持声明顺序() {
        let groups = groups();
        let keys: Vec<&str> = groups.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            vec![
                G_TERMINAL,
                G_NAV,
                G_LAYOUT,
                G_GLOBAL,
                G_MARKER,
                G_CLIPBOARD,
                G_VIEW
            ]
        );
        assert_eq!(groups[0].1[0].id, "newTerminal");
        assert_eq!(groups[2].1.len(), 6, "页签左右移两条 + 调比例四条");
        assert_eq!(groups[4].1.len(), 2, "AI 任务标记组两条");
        assert_eq!(groups[5].1.len(), 2, "剪贴板组两条");
        assert_eq!(groups[6].1.len(), 7, "回看四条 + 缩放三条");
    }

    /// 这一批新键位绑在 `Workspace` 上,且一条都不落在终端自己消费的回看键里 ——
    /// Ctrl+Shift+PgUp/PgDn 与回看的 Shift+PgUp/PgDn 只差一个 Ctrl,对不上就会
    /// 「按了翻页结果页签跑了」或反过来。
    #[test]
    fn 页签与分屏键位绑在工作区且不抢回看键() {
        use mt_ui::terminal::scrollback_key;

        let ids = [
            "closeTab",
            "toggleMaximize",
            "reopenClosed",
            "moveTabLeft",
            "moveTabRight",
            "resizeLeft",
            "resizeRight",
            "resizeUp",
            "resizeDown",
        ];
        for platform in [Platform::Other, Platform::Mac] {
            let bindings: Vec<KeyBinding> = ids
                .iter()
                .map(|id| {
                    let def = find(id).unwrap();
                    assert_eq!(def.scope, Scope::Global, "{id}");
                    binding_for(id, &default_keystroke_for(def, platform)).unwrap()
                })
                .collect();
            let keymap = Keymap::new(bindings);
            for id in ids {
                let keystroke = default_keystroke_for(find(id).unwrap(), platform);
                assert_eq!(
                    scrollback_key(&ks(&keystroke)),
                    None,
                    "{id} 撞上了终端回看键"
                );
                assert_eq!(
                    hits(&keymap, &keystroke, &in_terminal()).len(),
                    1,
                    "{id} 在终端里该命中"
                );
            }
        }
    }

    /// 回看四条登记的组合与 mt-ui 真正拦下的键位是同一组 —— 设置页写着 Shift+PgUp,
    /// 按下去就得是本地翻页。mac 上同样(⌘ 那一路 `scrollback_key` 不认,所以那两条保持 ⌃)。
    #[test]
    fn 回看键与终端实际拦截的键位一致() {
        use mt_ui::terminal::{ScrollbackKey, scrollback_key};
        for (id, expect) in [
            ("scrollPageUp", ScrollbackKey::PageUp),
            ("scrollPageDown", ScrollbackKey::PageDown),
            ("scrollTop", ScrollbackKey::Top),
            ("scrollBottom", ScrollbackKey::Bottom),
        ] {
            let def = find(id).unwrap();
            assert_eq!(def.scope, Scope::Terminal, "{id} 由终端自己消费");
            for platform in [Platform::Other, Platform::Mac] {
                let keystroke = default_keystroke_for(def, platform);
                assert_eq!(
                    scrollback_key(&ks(&keystroke)),
                    Some(expect),
                    "{id} {platform:?}"
                );
            }
        }
    }

    /// 剪贴板两条的显示与 `TerminalView::on_key_down` 的判定一致。那一支判据是内联在
    /// `on_key_down` 里的 `(control || platform) && shift` + `c` / `v`(`mt-ui/src/terminal/
    /// view.rs`),没有单独的纯函数可调,这里照抄那一行对照 —— 改了那边记得改这里。
    #[test]
    fn 剪贴板键与终端实际拦截的键位一致() {
        for (id, key) in [("copySelection", "c"), ("pasteToTerminal", "v")] {
            let def = find(id).unwrap();
            assert_eq!(def.scope, Scope::Terminal);
            for platform in [Platform::Other, Platform::Mac] {
                let parsed = ks(&default_keystroke_for(def, platform));
                let m = parsed.modifiers;
                assert!(
                    (m.control || m.platform) && m.shift && !m.alt,
                    "{id} {platform:?}"
                );
                assert_eq!(parsed.key, key);
            }
        }
    }

    /// 缩放三条只在终端里命中:焦点在终端时匹配得到 action,焦点在终端之外(文件查看器 /
    /// 面板,context 栈里没有 `Terminal`)一条都匹配不到,按键照常归那里。
    #[test]
    fn 缩放键只在终端里生效() {
        for platform in [Platform::Other, Platform::Mac] {
            let defs: Vec<&HotkeyDef> = HOTKEYS
                .iter()
                .filter(|d| d.scope == Scope::TerminalAction)
                .collect();
            assert_eq!(defs.len(), 3);
            let keymap = Keymap::new(
                defs.iter()
                    .filter_map(|d| binding_for(d.id, &default_keystroke_for(d, platform)))
                    .collect(),
            );
            let outside = contexts(&["Root", WORKSPACE]);
            for def in defs {
                let keystroke = default_keystroke_for(def, platform);
                assert_eq!(
                    hits(&keymap, &keystroke, &in_terminal()).len(),
                    1,
                    "{keystroke}"
                );
                assert!(
                    hits(&keymap, &keystroke, &outside).is_empty(),
                    "{keystroke}"
                );
            }
        }
    }

    /// 终端里的 Tab / Shift+Tab 必须**匹配不出任何 action** —— 只有一条 binding 都
    /// 没匹配上,gpui 才会走到 `finish_dispatch_key_event`,按键才到得了
    /// `TerminalView::on_key_down` 翻成 `\t` / `ESC [ Z`。
    ///
    /// 复刻的是真实注册顺序:`gpui_component::init` 先绑 `Root` 的 tab/shift-tab
    /// (`focus_next` / `focus_prev`),`bind_keys` 后绑我们的 `NoAction`。
    #[test]
    fn 终端里的_tab_压得过组件库的焦点导航() {
        let keymap = full_keymap(Platform::Other);
        let in_terminal = contexts(&["Root", TERMINAL]);
        for keystroke in ["tab", "shift-tab"] {
            assert!(
                hits(&keymap, keystroke, &in_terminal).is_empty(),
                "{keystroke} 在终端里不该匹配到 action"
            );
        }
        // 反面:终端之外(文件树 / 面板)照旧是组件库的焦点导航,别把 Tab 一并废掉
        assert_eq!(hits(&keymap, "tab", &contexts(&["Root"])).len(), 1);
    }

    /// **解绑或改走之后,旧键位真正落回终端**:在终端里一条绑定都匹配不上(gpui 才会把这次
    /// 按键交给 `TerminalView::on_key_down`)。拿真实的表与内部绑定建 keymap,按设置页那条
    /// 路(`rebind_bindings`)追加,覆盖全局动作与挂在 `Terminal` 上的缩放两种。
    #[test]
    fn 解绑或改走之后旧键位落回终端() {
        let platform = Platform::Other;
        let mut keymap = full_keymap(platform);
        let terminal = in_terminal();
        let search = TerminalSearch.name();
        let close_tab = CloseTerminalTab.name();
        let zoom_out = TerminalZoomOut.name();
        assert_eq!(hits(&keymap, "ctrl-f", &terminal), [search]);
        assert_eq!(hits(&keymap, "ctrl-shift-x", &terminal), [close_tab]);
        assert_eq!(hits(&keymap, "ctrl--", &terminal), [zoom_out]);

        let custom = overrides(&[
            ("terminalSearch", ""),
            ("closeTab", "ctrl-alt-x"),
            ("zoomOut", ""),
        ]);
        keymap.add_bindings(rebind_bindings(&Overrides::default(), &custom, platform));
        for old in ["ctrl-f", "ctrl-shift-x", "ctrl--"] {
            assert!(
                hits(&keymap, old, &terminal).is_empty(),
                "{old} 解绑 / 改走之后在终端里不该再匹配到动作"
            );
        }
        assert_eq!(hits(&keymap, "ctrl-alt-x", &terminal), [close_tab]);
        // `Unbind` 只解掉这个动作:Git 历史区里自己那条 Ctrl+F(聚焦过滤框)照旧
        let history = contexts(&["Root", WORKSPACE, git_history::HISTORY_CONTEXT]);
        assert_eq!(
            hits(&keymap, "ctrl-f", &history),
            [git_history::GitLogFocusFilter.name()]
        );

        // 改回默认:旧键位重新只命中一条(不会叠出两条同动作),新键位放开
        keymap.add_bindings(rebind_bindings(&custom, &Overrides::default(), platform));
        assert_eq!(hits(&keymap, "ctrl-f", &terminal), [search]);
        assert_eq!(hits(&keymap, "ctrl-shift-x", &terminal), [close_tab]);
        assert_eq!(hits(&keymap, "ctrl--", &terminal), [zoom_out]);
        assert!(hits(&keymap, "ctrl-alt-x", &terminal).is_empty());
    }

    /// 「替换」:A 抢走 B 的键位、B 解绑,一批追加里先解后绑 —— A 的新绑定不被误伤;
    /// 再把两条互换一次也各归各位。
    #[test]
    fn 替换与互换之后各键只命中一条() {
        let platform = Platform::Other;
        let mut keymap = full_keymap(platform);
        let terminal = in_terminal();
        let right = SplitRight.name();
        let down = SplitDown.name();

        let replaced = overrides(&[("splitRight", "ctrl-shift-e"), ("splitDown", "")]);
        keymap.add_bindings(rebind_bindings(&Overrides::default(), &replaced, platform));
        assert_eq!(hits(&keymap, "ctrl-shift-e", &terminal), [right]);
        assert!(hits(&keymap, "ctrl-shift-d", &terminal).is_empty());

        let swapped = overrides(&[
            ("splitRight", "ctrl-shift-e"),
            ("splitDown", "ctrl-shift-d"),
        ]);
        keymap.add_bindings(rebind_bindings(&replaced, &swapped, platform));
        assert_eq!(hits(&keymap, "ctrl-shift-e", &terminal), [right]);
        assert_eq!(hits(&keymap, "ctrl-shift-d", &terminal), [down]);
    }

    /// 读配置:认不出的 id、不可改的条目、读不懂 / 不合法的键位串一律丢弃并说明;
    /// 空串 = 解绑;写法不规范的规范化。
    #[test]
    fn 覆盖表读配置时丢弃认不出与读不懂的() {
        let raw: BTreeMap<String, String> = [
            ("newTerminal", "ctrl-alt-n"),
            ("terminalSearch", ""),
            ("quickOpen", "Shift-Alt-o"),
            ("someFutureAction", "ctrl-alt-q"),
            ("copySelection", "ctrl-alt-c"),
            ("selectPaneN", "alt-1"),
            ("closeTab", "ctrl-k ctrl-x"),
            ("splitRight", "q"),
            ("splitDown", "not-a-key-"),
            ("toggleSidebar", "win-b"),
        ]
        .iter()
        .map(|(id, keystroke)| (id.to_string(), keystroke.to_string()))
        .collect();
        let (parsed, issues) = Overrides::parse(&raw, Platform::Other);
        assert_eq!(issues.len(), 7, "{issues:?}");
        let mut expected = BTreeMap::new();
        expected.insert("newTerminal", Some("ctrl-alt-n".to_string()));
        expected.insert("terminalSearch", None);
        expected.insert("quickOpen", Some("alt-shift-o".to_string()));
        assert_eq!(parsed, Overrides(expected));
        let none = Overrides::default();
        assert_eq!(
            effective_keystroke_for(find("closeTab").unwrap(), &parsed, Platform::Other),
            effective_keystroke_for(find("closeTab").unwrap(), &none, Platform::Other),
            "丢掉的条目按默认键位"
        );
        assert_eq!(
            effective_keystroke_for(find("terminalSearch").unwrap(), &parsed, Platform::Other),
            None
        );
    }

    /// 写覆盖表:与默认相同就删(只存不同的项),解绑存空串,认不出 / 不可改的不动;
    /// 恢复默认(含「全部」)只清认得的条目,新版本留下的原样保留。
    #[test]
    fn 覆盖表只存与默认不同的项() {
        let platform = Platform::Other;
        let mut raw = BTreeMap::new();
        raw.insert("someFutureAction".to_string(), "ctrl-alt-q".to_string());

        write_override(&mut raw, "newTerminal", Some("ctrl-alt-n"), platform);
        assert_eq!(
            raw.get("newTerminal").map(String::as_str),
            Some("ctrl-alt-n")
        );
        write_override(&mut raw, "newTerminal", Some("Shift-Ctrl-T"), platform);
        assert!(
            !raw.contains_key("newTerminal"),
            "改回默认键位 = 删掉这一项"
        );
        write_override(&mut raw, "terminalSearch", None, platform);
        assert_eq!(raw.get("terminalSearch").map(String::as_str), Some(""));
        write_override(&mut raw, "copySelection", Some("ctrl-alt-c"), platform);
        write_override(&mut raw, "没有这个键位", Some("ctrl-alt-c"), platform);
        assert!(!raw.contains_key("copySelection") && !raw.contains_key("没有这个键位"));

        // mac 上的默认是 ⌘⇧T:写回 cmd-shift-t 同样等于恢复默认
        let mut mac = BTreeMap::new();
        write_override(&mut mac, "newTerminal", Some("cmd-shift-t"), Platform::Mac);
        assert!(mac.is_empty());

        write_override(&mut raw, "zoomIn", Some("ctrl-alt-="), platform);
        reset_overrides(&mut raw, Some("terminalSearch"));
        assert!(!raw.contains_key("terminalSearch"));
        assert!(raw.contains_key("zoomIn"));
        reset_overrides(&mut raw, None);
        assert_eq!(raw.keys().collect::<Vec<_>>(), ["someFutureAction"]);
    }

    /// 录制时一次按键的判定:单独修饰键接着等、裸 Esc 取消、裸 Backspace / Delete 解绑;
    /// 必须带 Ctrl / Alt(mac 上还有 ⌘)或是 F1–F12;系统 / 输入法占用的、AltGr 打字的不收。
    #[test]
    fn 录制时的按键判定() {
        let other = Platform::Other;
        let mac = Platform::Mac;
        let rec = |keystroke: &str, platform| interpret_recorded(&ks(keystroke), platform);

        for modifier in ["shift", "control", "alt", "platform"] {
            assert_eq!(rec(modifier, other), Recorded::Pending, "{modifier}");
        }
        assert_eq!(rec("escape", other), Recorded::Cancel);
        assert_eq!(rec("backspace", other), Recorded::Unbind);
        assert_eq!(rec("delete", mac), Recorded::Unbind);

        for bare in [
            "a",
            "enter",
            "tab",
            "space",
            "shift-a",
            "shift-enter",
            "shift-escape",
            "up",
            "pageup",
            "f13",
        ] {
            assert_eq!(
                rec(bare, other),
                Recorded::Invalid(Invalid::NeedsModifier),
                "{bare}"
            );
        }
        assert_eq!(rec("f5", other), Recorded::Combo("f5".into()));
        assert_eq!(rec("shift-f12", other), Recorded::Combo("shift-f12".into()));
        assert_eq!(
            rec("ctrl-alt-t", other),
            Recorded::Combo("ctrl-alt-t".into())
        );
        assert_eq!(
            rec("alt-backspace", other),
            Recorded::Combo("alt-backspace".into())
        );

        // Win 键归系统;mac 的 ⌘ 是正经修饰键
        assert_eq!(rec("win-t", other), Recorded::Invalid(Invalid::SystemKey));
        assert_eq!(
            rec("cmd-shift-t", mac),
            Recorded::Combo("cmd-shift-t".into())
        );

        for reserved in ["alt-tab", "alt-f4", "ctrl-space", "ctrl-escape"] {
            assert_eq!(
                rec(reserved, other),
                Recorded::Invalid(Invalid::Reserved),
                "{reserved}"
            );
        }
        for reserved in [
            "cmd-q",
            "cmd-tab",
            "cmd-`",
            "ctrl-space",
            "cmd-w",
            "ctrl-left",
        ] {
            assert_eq!(
                rec(reserved, mac),
                Recorded::Invalid(Invalid::Reserved),
                "{reserved}"
            );
        }
        assert_eq!(rec("ctrl-left", other), Recorded::Combo("ctrl-left".into()));

        // AltGr:Windows 把它报成 Ctrl+Alt,但打出了字符
        let altgr = Keystroke {
            modifiers: Modifiers {
                control: true,
                alt: true,
                ..Modifiers::default()
            },
            key: "q".into(),
            key_char: Some("@".into()),
        };
        assert_eq!(
            interpret_recorded(&altgr, other),
            Recorded::Invalid(Invalid::TypesCharacter)
        );
        let real = Keystroke {
            key_char: None,
            ..altgr
        };
        assert_eq!(
            interpret_recorded(&real, other),
            Recorded::Combo("ctrl-alt-q".into())
        );
    }

    /// 录到的键位按占用情况裁决:另一条可改的 → 冲突(可替换);只读条目、终端自己消费的、
    /// 弹窗内部的 → 拒绝;看的是**生效**键位。
    #[test]
    fn 录到的键位按占用情况裁决() {
        let other = Platform::Other;
        let none = Overrides::default();
        let judge_on = |id, candidate, overrides: &Overrides, smart| {
            judge(id, candidate, overrides, other, smart)
        };
        assert_eq!(
            judge_on("newTerminal", "ctrl-shift-t", &none, true),
            Verdict::Unchanged
        );
        assert_eq!(
            judge_on("newTerminal", "ctrl-alt-n", &none, true),
            Verdict::Accept
        );
        assert_eq!(
            judge_on("newTerminal", "ctrl-shift-d", &none, true),
            Verdict::Conflict("splitRight")
        );
        // 全局动作与终端内的缩放:终端有焦点时都会命中,一样算冲突
        assert_eq!(
            judge_on("newTerminal", "ctrl-=", &none, true),
            Verdict::Conflict("zoomIn")
        );
        assert_eq!(
            judge_on("zoomIn", "ctrl-shift-t", &none, true),
            Verdict::Conflict("newTerminal")
        );

        // 只读条目与终端自己消费的键
        let named = |desc| Verdict::Taken(Owner::Named(desc));
        assert_eq!(
            judge_on("newTerminal", "ctrl-shift-c", &none, true),
            named("shortcuts.copySelected")
        );
        assert_eq!(
            judge_on("newTerminal", "ctrl-3", &none, true),
            named("shortcuts.selectPaneN")
        );
        assert_eq!(
            judge_on("newTerminal", "ctrl-shift-home", &none, true),
            named("shortcuts.scrollTop")
        );
        // 智能复制粘贴开着时 Ctrl+V 是终端的;关着时只是个普通的终端键(另有提醒)
        assert_eq!(
            judge_on("newTerminal", "ctrl-v", &none, true),
            named("shortcuts.pasteToTerminal")
        );
        assert_eq!(
            judge_on("newTerminal", "ctrl-v", &none, false),
            Verdict::Accept
        );

        // 弹窗 / 面板 / 页签内部的键位
        let internal = Verdict::Taken(Owner::Internal);
        for key in [
            "ctrl-enter",
            "f7",
            "ctrl-home",
            "ctrl-s",
            "ctrl-w",
            "ctrl-shift-i",
        ] {
            assert_eq!(judge_on("newTerminal", key, &none, true), internal, "{key}");
        }
        // 录回自己的默认键位不算撞内部键位(Alt+← 与 diff 页签的 Alt+← 并存是原设计)
        let unbound = overrides(&[("focusLeft", "")]);
        assert_eq!(
            judge_on("focusLeft", "alt-left", &unbound, true),
            Verdict::Accept
        );
        assert_eq!(judge_on("resizeLeft", "alt-left", &unbound, true), internal);

        // 别人改走了,它原来的默认键位就空出来了
        let moved = overrides(&[("splitRight", "ctrl-alt-d")]);
        assert_eq!(
            judge_on("newTerminal", "ctrl-shift-d", &moved, true),
            Verdict::Accept
        );
        assert_eq!(
            judge_on("newTerminal", "ctrl-alt-d", &moved, true),
            Verdict::Conflict("splitRight")
        );

        // mac
        let mac = Platform::Mac;
        assert_eq!(
            judge("newTerminal", "cmd-shift-d", &none, mac, true),
            Verdict::Conflict("splitRight")
        );
        assert_eq!(
            judge("newTerminal", "cmd-c", &none, mac, false),
            named("shortcuts.copyDesc"),
            "mac 上 ⌘C 不受智能开关管"
        );
        assert_eq!(
            judge("newTerminal", "ctrl-shift-c", &none, mac, true),
            named("shortcuts.copySelected"),
            "终端的剪贴板分支 ⌃ 与 ⌘ 同权"
        );
        assert_eq!(
            judge("newTerminal", "cmd-left", &none, mac, true),
            named("shortcuts.lineStartEnd")
        );
        assert_eq!(
            judge("newTerminal", "cmd-5", &none, mac, true),
            named("shortcuts.selectPaneN")
        );
        assert_eq!(
            judge("newTerminal", "cmd-s", &none, mac, true),
            Verdict::Taken(Owner::Internal)
        );
    }

    /// 占用终端常用键的提醒:Ctrl+字母(不带 Shift)、Ctrl+几个控制码标点、Alt+单字符。
    #[test]
    fn 终端常用键给提醒() {
        for keystroke in [
            "ctrl-r", "ctrl-c", "ctrl-z", "ctrl-a", "ctrl-]", "alt-b", "alt-.",
        ] {
            assert!(terminal_warning(keystroke), "{keystroke}");
        }
        for keystroke in [
            "ctrl-shift-r",
            "ctrl-alt-r",
            "ctrl-tab",
            "ctrl-1",
            "cmd-r",
            "f2",
            "alt-left",
            "alt-shift-b",
        ] {
            assert!(!terminal_warning(keystroke), "{keystroke}");
        }
    }

    /// AI 任务标记(audit #25)的两条键位齐了 —— W 批把功能补上,这条断言随之
    /// **翻面**:R 批当时钉的是「还没实现就不许列出来」,现在钉的是「实现了就必须
    /// 列出来」,两个方向守的是同一条红线(设置页写着能按 ⇔ 按了真有反应)。
    #[test]
    fn 标记跳转两条键位都在表里() {
        let ids: Vec<&str> = HOTKEYS
            .iter()
            .filter(|d| d.id.starts_with("marker"))
            .map(|d| d.id)
            .collect();
        assert_eq!(ids, vec!["markerPrev", "markerNext"]);
        // 与原版 `hotkeys.ts:73-74` 同键位:Ctrl+Shift+↑ / ↓
        for def in HOTKEYS.iter().filter(|d| d.id.starts_with("marker")) {
            let m = ks(def.keystroke).modifiers;
            assert!(m.control && m.shift && !m.alt, "{}", def.id);
            assert_eq!(def.group_key, G_MARKER);
        }
    }

    /// 按 id 取显示串 / 描述 key:表里有就取到,没有则空 / None;解绑了显示串是空,
    /// tooltip 用的那份是「未设置」。
    #[test]
    fn 按_id_取显示串与描述key() {
        let platform = Platform::current();
        assert_eq!(
            hotkey_label("newTerminal"),
            label_for(&HOTKEYS[0], &Overrides::default(), platform).unwrap()
        );
        assert_eq!(hotkey_desc_key("newTerminal"), Some("shortcuts.newTerminal"));
        assert_eq!(hotkey_desc_key("switchProject"), Some("shortcuts.switchProject"));
        assert_eq!(hotkey_desc_key("terminalSearch"), Some("shortcuts.terminalSearch"));
        // 认不出的 id 不炸(原版 `hotkeys[id]` 取 undefined 也不炸)
        assert_eq!(hotkey_label("没有这个键位"), "");
        assert_eq!(hotkey_desc_key("没有这个键位"), None);

        // 改过 / 解绑之后跟着生效值走(APPLIED 是线程本地的,不串到别的测试)
        APPLIED.with(|applied| {
            *applied.borrow_mut() = overrides_on(
                &[("terminalSearch", ""), ("newTerminal", "ctrl-alt-n")],
                platform,
            )
        });
        assert_eq!(hotkey_label("terminalSearch"), "");
        assert_eq!(
            hotkey_tip("terminalSearch"),
            crate::i18n::t("settings", "shortcuts.unbound")
        );
        assert_eq!(hotkey_label("newTerminal"), keystroke_label("ctrl-alt-n"));
        assert_eq!(hotkey_tip("newTerminal"), keystroke_label("ctrl-alt-n"));
        APPLIED.with(|applied| *applied.borrow_mut() = Overrides::default());
    }

    /// 显示串的拼法:Windows / Linux 与改造前一字不差;mac 上 ⌘ / ⌃ 分开画、无分隔符。
    #[test]
    fn 显示串拼法() {
        let other = Platform::Other;
        assert_eq!(keystroke_label_for("ctrl-shift-t", other), "Ctrl+Shift+T");
        assert_eq!(keystroke_label_for("alt-up", other), "Alt+↑");
        assert_eq!(keystroke_label_for("f2", other), "F2");
        assert_eq!(keystroke_label_for("alt-shift-left", other), "Shift+Alt+←");
        assert_eq!(
            keystroke_label_for("ctrl-shift-pagedown", other),
            "Ctrl+Shift+PgDn"
        );
        assert_eq!(
            keystroke_label_for("ctrl-shift-home", other),
            "Ctrl+Shift+Home"
        );
        assert_eq!(keystroke_label_for("ctrl-`", other), "Ctrl+`");
        let none = Overrides::default();
        let label = |id, platform| label_for(find(id).unwrap(), &none, platform);
        assert_eq!(label("selectPaneN", other).as_deref(), Some("Ctrl+1…9"));
        assert_eq!(label("resizeLeft", other).as_deref(), Some("Shift+Alt+←"));

        let mac = Platform::Mac;
        assert_eq!(label("newTerminal", mac).as_deref(), Some("⌘⇧T"));
        assert_eq!(label("nextPane", mac).as_deref(), Some("⌃Tab"));
        assert_eq!(label("selectPaneN", mac).as_deref(), Some("⌘1…9"));
        assert_eq!(label("scrollTop", mac).as_deref(), Some("⌃⇧Home"));
        assert_eq!(label("copySelection", mac).as_deref(), Some("⌘⇧C"));
        assert_eq!(label("resizeLeft", mac).as_deref(), Some("⇧⌥←"));
        assert_eq!(label("openSettings", mac).as_deref(), Some("⌘,"));
        assert_eq!(label("zoomOut", mac).as_deref(), Some("⌘-"));
        assert_eq!(keystroke_label_for("ctrl-c", mac), "⌃C");
    }
}
