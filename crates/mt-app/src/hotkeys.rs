//! 全应用快捷键的**唯一事实来源**。对应 `src/utils/hotkeys.ts`。
//!
//! 这张表同时被两处消费:
//!
//! 1. [`bind_keys`] —— 启动时往 gpui 注册 `KeyBinding`(原来是 `main.rs` 里
//!    一串裸 `KeyBinding::new`,没有分组也没有描述 key);
//! 2. 设置 →「快捷键」页 —— 按 [`groups`] 分组渲染。
//!
//! 原版把这两件事分开写过一阵,结论写在 `SettingsModal.tsx:1659-1663` 的注释里:
//! 「改了键位忘改说明就直接漂移」。所以这里同样收成一张表,而且用
//! [`tests::显示串与实际键位一致`] 把「显示串」与「真正绑上去的键位串」钉在一起。
//!
//! # 键位选择原则(照抄 `hotkeys.ts` 的开头注释)
//!
//! 终端应用的第一约束是**不能吞掉 shell / TUI 需要的按键**。裸 `Ctrl+T`
//! (bash transpose-chars)、`Ctrl+W`(删除前一个词)、`Ctrl+P`(上一条历史)都有
//! 既定语义,因此应用级动作统一走 `Ctrl+Shift+*`(Windows Terminal / VS Code
//! 终端的惯例),只有确实不与行编辑冲突的才用裸 `Ctrl`
//! (`Ctrl+Tab`、`Ctrl+1..9`、`Ctrl+,`)。
//!
//! # 与原版表的两处差异(都有意)
//!
//! 1. **多出 GPUI 独有的三条**(`ctrl-shift-a` / `u` / `j`):原版边栏没有这几个
//!    面板,描述文案是 R 批往 TS 源头补的;
//! 2. 剪贴板那两条(`Ctrl+Shift+C/V`)在原版是「xterm 的 customKeyEventHandler 消费,
//!    表里只为设置页展示」,GPUI 侧同样如此 —— 由 `mt_ui::TerminalView::on_key_down`
//!    自己吃掉,这里 `keystroke = None`,**不绑 action**(绑了就轮不到终端)。

use gpui::{App, KeyBinding};

use crate::{
    ClosePane, FocusDown, FocusLeft, FocusRight, FocusUp, GlobalSearch, JumpAttention, MarkerNext,
    MarkerPrev, NewTerminal, NextPane, OpenTerminalSettings, PrevPane, RenamePane, SelectPane,
    SplitDown, SplitRight, SwitchProject, TerminalSearch, ToggleMiddleColumn, ToggleSessions,
    ToggleUsage, git_changes, project_switcher,
};

/// 应用级动作的 key context(与 `Workspace::render` 的 `key_context` 一致)。
const WORKSPACE: &str = "Workspace";

/// 快捷键作用域。决定按键在什么情况下被拦截 —— 与 `hotkeys.ts::HotkeyScope` 同义。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// 全局:绑成 gpui action,终端看不到这些键。
    Global,
    /// 终端内:由 `mt_ui::TerminalView` 自己消费,这里只为设置页展示。
    Terminal,
}

/// 按键组合的**显示**形态。`key` 已经是给人看的那一段(`T` / `Tab` / `↑` / `1…9`)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Combo {
    /// Ctrl(mac 上是 ⌘)
    pub modifier: bool,
    pub shift: bool,
    pub alt: bool,
    pub key: &'static str,
}

pub struct HotkeyDef {
    pub id: &'static str,
    /// gpui 键位串(`KeyBinding::new` 的第一个参数)。
    ///
    /// `None` 有两种情形:① 终端 scope(不绑 action);
    /// ② `Ctrl+1..9` 这种一条表项对应九个绑定的,由 [`bind_keys`] 单独展开。
    pub keystroke: Option<&'static str>,
    pub combo: Combo,
    /// 只在 [`tests::全局条目都绑得上_action`] 里读:它把「终端 scope 绝不能绑
    /// action」这条钉死(绑了就轮不到终端自己消费 Ctrl+Shift+C/V)。
    #[cfg_attr(not(test), allow(dead_code))]
    pub scope: Scope,
    /// 设置页分组标题的 i18n key(`settings` 命名空间内的相对 key)。
    pub group_key: &'static str,
    /// 动作描述的 i18n key(同上)。
    pub desc_key: &'static str,
}

const G_TERMINAL: &str = "shortcuts.terminalOps";
const G_NAV: &str = "shortcuts.navigation";
const G_GLOBAL: &str = "shortcuts.global";
const G_MARKER: &str = "shortcuts.aiTaskMarks";
const G_CLIPBOARD: &str = "shortcuts.clipboard";

/// 便捷构造:大多数条目只差修饰键与键名。
const fn def(
    id: &'static str,
    keystroke: Option<&'static str>,
    combo: Combo,
    scope: Scope,
    group_key: &'static str,
    desc_key: &'static str,
) -> HotkeyDef {
    HotkeyDef {
        id,
        keystroke,
        combo,
        scope,
        group_key,
        desc_key,
    }
}

const fn combo(modifier: bool, shift: bool, alt: bool, key: &'static str) -> Combo {
    Combo {
        modifier,
        shift,
        alt,
        key,
    }
}

pub const HOTKEYS: &[HotkeyDef] = &[
    // ── 终端操作 ──
    def(
        "newTerminal",
        Some("ctrl-shift-t"),
        combo(true, true, false, "T"),
        Scope::Global,
        G_TERMINAL,
        "shortcuts.newTerminal",
    ),
    def(
        "closePane",
        Some("ctrl-shift-w"),
        combo(true, true, false, "W"),
        Scope::Global,
        G_TERMINAL,
        "shortcuts.closePane",
    ),
    def(
        "renamePane",
        Some("f2"),
        combo(false, false, false, "F2"),
        Scope::Global,
        G_TERMINAL,
        "shortcuts.renamePane",
    ),
    def(
        "splitRight",
        Some("ctrl-shift-d"),
        combo(true, true, false, "D"),
        Scope::Global,
        G_TERMINAL,
        "shortcuts.splitRight",
    ),
    def(
        "splitDown",
        Some("ctrl-shift-e"),
        combo(true, true, false, "E"),
        Scope::Global,
        G_TERMINAL,
        "shortcuts.splitDown",
    ),
    // ── 导航 ──
    def(
        "nextPane",
        Some("ctrl-tab"),
        combo(true, false, false, "Tab"),
        Scope::Global,
        G_NAV,
        "shortcuts.nextPane",
    ),
    def(
        "prevPane",
        Some("ctrl-shift-tab"),
        combo(true, true, false, "Tab"),
        Scope::Global,
        G_NAV,
        "shortcuts.prevPane",
    ),
    // 一条表项 → 九个绑定,`keystroke` 留空由 bind_keys 展开
    // (原版这里存的也是占位串 `'1…9'`)
    def(
        "selectPaneN",
        None,
        combo(true, false, false, "1…9"),
        Scope::Global,
        G_NAV,
        "shortcuts.selectPaneN",
    ),
    def(
        "focusLeft",
        Some("alt-left"),
        combo(false, false, true, "←"),
        Scope::Global,
        G_NAV,
        "shortcuts.focusLeft",
    ),
    def(
        "focusRight",
        Some("alt-right"),
        combo(false, false, true, "→"),
        Scope::Global,
        G_NAV,
        "shortcuts.focusRight",
    ),
    def(
        "focusUp",
        Some("alt-up"),
        combo(false, false, true, "↑"),
        Scope::Global,
        G_NAV,
        "shortcuts.focusUp",
    ),
    def(
        "focusDown",
        Some("alt-down"),
        combo(false, false, true, "↓"),
        Scope::Global,
        G_NAV,
        "shortcuts.focusDown",
    ),
    // ── 全局 ──
    def(
        "switchProject",
        Some("ctrl-shift-p"),
        combo(true, true, false, "P"),
        Scope::Global,
        G_GLOBAL,
        "shortcuts.switchProject",
    ),
    def(
        "globalSearch",
        Some("ctrl-shift-f"),
        combo(true, true, false, "F"),
        Scope::Global,
        G_GLOBAL,
        "shortcuts.toggleGlobalSearch",
    ),
    def(
        "terminalSearch",
        Some("ctrl-f"),
        combo(true, false, false, "F"),
        Scope::Global,
        G_GLOBAL,
        "shortcuts.terminalSearch",
    ),
    def(
        "openSettings",
        Some("ctrl-,"),
        combo(true, false, false, ","),
        Scope::Global,
        G_GLOBAL,
        "shortcuts.openSettings",
    ),
    def(
        "toggleSidebar",
        Some("ctrl-shift-b"),
        combo(true, true, false, "B"),
        Scope::Global,
        G_GLOBAL,
        "shortcuts.toggleSidebar",
    ),
    // GPUI 独有的三条(原版边栏没有这几个面板)
    def(
        "toggleSessions",
        Some("ctrl-shift-a"),
        combo(true, true, false, "A"),
        Scope::Global,
        G_GLOBAL,
        "shortcuts.toggleSessions",
    ),
    def(
        "toggleUsage",
        Some("ctrl-shift-u"),
        combo(true, true, false, "U"),
        Scope::Global,
        G_GLOBAL,
        "shortcuts.toggleUsage",
    ),
    def(
        "jumpAttention",
        Some("ctrl-shift-j"),
        combo(true, true, false, "J"),
        Scope::Global,
        G_GLOBAL,
        "shortcuts.jumpAttention",
    ),
    // ── AI 任务标记(`hotkeys.ts:73-74`)──
    //
    // 键名 `up`/`down` 与项目切换器那两条一致。原版这两条**不走 useGlobalHotkeys**
    // (那边显式把它们排除),自己挂 capture 阶段的 window 监听,于是绕过了
    // `isTypingTarget` 与 overlay 两道闸;GPUI 侧照常走 `yields_to_overlay` ——
    // 方向键在输入框里有明确语义,在设置对话框里按 Ctrl+Shift+↑ 跳终端是意外行为。
    // 这是刻意偏差,见 `main.rs::Workspace::on_marker_prev`。
    def(
        "markerPrev",
        Some("ctrl-shift-up"),
        combo(true, true, false, "↑"),
        Scope::Global,
        G_MARKER,
        "shortcuts.jumpPrevAi",
    ),
    def(
        "markerNext",
        Some("ctrl-shift-down"),
        combo(true, true, false, "↓"),
        Scope::Global,
        G_MARKER,
        "shortcuts.jumpNextAi",
    ),
    // ── 剪贴板(终端内,由 TerminalView 消费;这里只为设置页展示)──
    def(
        "copySelection",
        None,
        combo(true, true, false, "C"),
        Scope::Terminal,
        G_CLIPBOARD,
        "shortcuts.copySelected",
    ),
    def(
        "pasteToTerminal",
        None,
        combo(true, true, false, "V"),
        Scope::Terminal,
        G_CLIPBOARD,
        "shortcuts.pasteToTerminal",
    ),
];

/// 单条快捷键的显示串,如 `Ctrl+Shift+T` / `⌘⇧T`。
/// 逐条对照 `hotkeys.ts::comboLabel`:mac 用 `⌘⇧⌥` 无分隔符拼,其余 `+` 连接。
pub fn combo_label(combo: &Combo) -> String {
    #[cfg(target_os = "macos")]
    let (m, s, a, sep) = ("⌘", "⇧", "⌥", "");
    #[cfg(not(target_os = "macos"))]
    let (m, s, a, sep) = ("Ctrl", "Shift", "Alt", "+");

    let mut parts: Vec<&str> = Vec::new();
    if combo.modifier {
        parts.push(m);
    }
    if combo.shift {
        parts.push(s);
    }
    if combo.alt {
        parts.push(a);
    }
    parts.push(combo.key);
    parts.join(sep)
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

/// 把表注册进 gpui。**启动时只调一次**(`main.rs`)。
///
/// gpui 的按键派发**先匹配 action 绑定、后跑 key 监听**,所以这里绑上就等于拿到了
/// 原版 capture 阶段 `consume(e)` 的效果:终端看不到这些键。
pub fn bind_keys(cx: &mut App) {
    let mut bindings: Vec<KeyBinding> = Vec::new();
    for def in HOTKEYS {
        if let Some(ks) = def.keystroke
            && let Some(binding) = binding_for(def.id, ks)
        {
            bindings.push(binding);
        }
    }

    // Ctrl+1..9:选中叶内第 N 个 tab(表里是一条占位项 `1…9`,这里展开成九条)
    for n in 1..=9usize {
        bindings.push(KeyBinding::new(
            &format!("ctrl-{n}"),
            SelectPane(n),
            Some(WORKSPACE),
        ));
    }

    // 项目切换器的方向键。**不进快捷键表** —— 它是弹窗内部键位,设置页不该列。
    //
    // 谓词写成 `"ProjectSwitcher > Input"` 而不是 `"ProjectSwitcher"`:只有与
    // `Input` **同深度**才压得过组件库自带的 `up`/`down`(单行输入框那两个处理器
    // 直接 return、不 propagate,挂在容器上的 on_key_down 永远收不到)。
    // 详见 project_switcher 模块注释,机制有单测钉住。
    bindings.push(KeyBinding::new(
        "up",
        project_switcher::SwitcherPrev,
        Some("ProjectSwitcher > Input"),
    ));
    bindings.push(KeyBinding::new(
        "down",
        project_switcher::SwitcherNext,
        Some("ProjectSwitcher > Input"),
    ));

    // Git 提交框的 Ctrl/Cmd+Enter(`GitChanges.tsx:411-415`)。**不进快捷键表** ——
    // 原版它是 textarea 的 onKeyDown,不在 hotkeys.ts 里。谓词同上要与 `Input` 同深度。
    bindings.push(KeyBinding::new(
        "ctrl-enter",
        git_changes::GitCommitMessage,
        Some("GitChanges > Input"),
    ));
    bindings.push(KeyBinding::new(
        "cmd-enter",
        git_changes::GitCommitMessage,
        Some("GitChanges > Input"),
    ));

    cx.bind_keys(bindings);
}

/// id → action 的分派。**新增表项时这里也要加一条**,否则那条键位静默绑不上
/// (设置页照样列出来 = 又一次「写着能按、按了没反应」)。
///
/// 返回 `None` 只应发生在终端 scope 的条目上,而它们的 `keystroke` 本就是 `None`,
/// 走不到这里 —— 有 [`tests::全局条目都绑得上_action`] 盯着。
fn binding_for(id: &str, keystroke: &str) -> Option<KeyBinding> {
    Some(match id {
        "newTerminal" => KeyBinding::new(keystroke, NewTerminal, Some(WORKSPACE)),
        "closePane" => KeyBinding::new(keystroke, ClosePane, Some(WORKSPACE)),
        "renamePane" => KeyBinding::new(keystroke, RenamePane, Some(WORKSPACE)),
        "splitRight" => KeyBinding::new(keystroke, SplitRight, Some(WORKSPACE)),
        "splitDown" => KeyBinding::new(keystroke, SplitDown, Some(WORKSPACE)),
        "nextPane" => KeyBinding::new(keystroke, NextPane, Some(WORKSPACE)),
        "prevPane" => KeyBinding::new(keystroke, PrevPane, Some(WORKSPACE)),
        "focusLeft" => KeyBinding::new(keystroke, FocusLeft, Some(WORKSPACE)),
        "focusRight" => KeyBinding::new(keystroke, FocusRight, Some(WORKSPACE)),
        "focusUp" => KeyBinding::new(keystroke, FocusUp, Some(WORKSPACE)),
        "focusDown" => KeyBinding::new(keystroke, FocusDown, Some(WORKSPACE)),
        "switchProject" => KeyBinding::new(keystroke, SwitchProject, Some(WORKSPACE)),
        "globalSearch" => KeyBinding::new(keystroke, GlobalSearch, Some(WORKSPACE)),
        "terminalSearch" => KeyBinding::new(keystroke, TerminalSearch, Some(WORKSPACE)),
        "openSettings" => KeyBinding::new(keystroke, OpenTerminalSettings, Some(WORKSPACE)),
        "toggleSidebar" => KeyBinding::new(keystroke, ToggleMiddleColumn, Some(WORKSPACE)),
        "toggleSessions" => KeyBinding::new(keystroke, ToggleSessions, Some(WORKSPACE)),
        "toggleUsage" => KeyBinding::new(keystroke, ToggleUsage, Some(WORKSPACE)),
        "jumpAttention" => KeyBinding::new(keystroke, JumpAttention, Some(WORKSPACE)),
        "markerPrev" => KeyBinding::new(keystroke, MarkerPrev, Some(WORKSPACE)),
        "markerNext" => KeyBinding::new(keystroke, MarkerNext, Some(WORKSPACE)),
        _ => return None,
    })
}

/// gpui 键位串里的键名 → 显示串(`hotkeys.ts::KEY_LABELS` 的等价物)。
///
/// 只被 [`tests::显示串与实际键位一致`] 用:表里的 `combo.key` 已经是显示形态,
/// 这个函数存在的意义是**从真正绑上去的键位串反推**一遍,两者对不上就红。
#[cfg_attr(not(test), allow(dead_code))]
pub fn key_label(key: &str) -> String {
    match key {
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        "tab" => "Tab".into(),
        "escape" => "Esc".into(),
        "enter" => "Enter".into(),
        // 单字符键与 F1..F12 一律大写(`t` → `T`,`f2` → `F2`)
        other => other.to_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 表里每条**有键位串**的项,显示串必须与真正绑上去的键位一致 ——
    /// 这就是原版注释里说的「改了键位忘改说明就漂移」的防线。
    #[test]
    fn 显示串与实际键位一致() {
        for def in HOTKEYS {
            let Some(ks) = def.keystroke else { continue };
            let parsed = gpui::Keystroke::parse(ks).unwrap_or_else(|e| panic!("{ks} 解析失败: {e}"));
            assert_eq!(
                parsed.modifiers.control, def.combo.modifier,
                "{} 的 Ctrl 对不上",
                def.id
            );
            assert_eq!(
                parsed.modifiers.shift, def.combo.shift,
                "{} 的 Shift 对不上",
                def.id
            );
            assert_eq!(parsed.modifiers.alt, def.combo.alt, "{} 的 Alt 对不上", def.id);
            assert_eq!(
                key_label(&parsed.key),
                def.combo.key,
                "{} 的键名对不上",
                def.id
            );
        }
    }

    /// 全局条目必须都能分派到 action —— 分派表漏一条 = 设置页列着但按不出效果。
    #[test]
    fn 全局条目都绑得上_action() {
        for def in HOTKEYS {
            match def.scope {
                Scope::Global => {
                    let Some(ks) = def.keystroke else {
                        // 唯一允许没有键位串的全局条目是 Ctrl+1..9(bind_keys 单独展开)
                        assert_eq!(def.id, "selectPaneN");
                        continue;
                    };
                    assert!(binding_for(def.id, ks).is_some(), "{} 没有 action", def.id);
                }
                // 终端 scope 绝不能绑 action:绑了就轮不到终端自己消费
                Scope::Terminal => assert!(def.keystroke.is_none(), "{} 不该绑键位", def.id),
            }
        }
    }

    /// 分组保持声明顺序、组内保持表内顺序(设置页照这个顺序画)。
    ///
    /// 组序与原版 `hotkeys.ts:42-46` 一致:终端 / 导航 / 全局 / AI 任务标记 / 剪贴板。
    #[test]
    fn 分组保持声明顺序() {
        let groups = groups();
        let keys: Vec<&str> = groups.iter().map(|(k, _)| *k).collect();
        assert_eq!(keys, vec![G_TERMINAL, G_NAV, G_GLOBAL, G_MARKER, G_CLIPBOARD]);
        assert_eq!(groups[0].1[0].id, "newTerminal");
        assert_eq!(groups[3].1.len(), 2, "AI 任务标记组两条");
        assert_eq!(groups[4].1.len(), 2, "剪贴板组两条");
    }

    /// id 不重复 —— 设置页拿 id 当行 key。
    #[test]
    fn id_不重复() {
        let mut ids: Vec<&str> = HOTKEYS.iter().map(|d| d.id).collect();
        ids.sort_unstable();
        let count = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), count);
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
            assert!(def.combo.modifier && def.combo.shift && !def.combo.alt, "{}", def.id);
            assert_eq!(def.group_key, G_MARKER);
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn 显示串拼法() {
        assert_eq!(combo_label(&combo(true, true, false, "T")), "Ctrl+Shift+T");
        assert_eq!(combo_label(&combo(false, false, true, "↑")), "Alt+↑");
        assert_eq!(combo_label(&combo(false, false, false, "F2")), "F2");
        assert_eq!(combo_label(&combo(true, false, false, "1…9")), "Ctrl+1…9");
    }
}
