//! 覆盖物栈 ——「当前屏幕上压着什么」的唯一真相。对应 `src/utils/overlayStack.ts`。
//!
//! # 原版为什么需要它
//!
//! 浏览器那边三类覆盖物(React 弹窗 / 命令式 prompt / 右键菜单)各自在 window 或
//! document 的 capture 上挂 Esc,派发顺序由注册节点决定而不是由「谁在最上面」决定 ——
//! 弹窗里再弹一个 confirm,Esc 关掉的是底下那层。同时 `useGlobalHotkeys` 靠 App 手工
//! 维护的 `modalOpen` 布尔判断「有没有弹窗」,漏掉了一多半覆盖物,查看器开着时
//! Ctrl+Shift+W 照样去关后面的终端。统一到一个栈之后:谁在栈顶谁吃 Esc,
//! `isOverlayOpen()` 是全局唯一判据。
//!
//! # GPUI 侧只剩一半工作
//!
//! **Esc 归谁**在 GPUI 里是结构性免费的:按键沿**焦点链**派发,浮层开着时焦点就在
//! 浮层上(`Dialog` 自己 `track_focus`,`menu.rs` 打开时把焦点收走),最上层那个天然
//! 第一个收到 Esc。所以这里不复刻「Esc 只问栈顶」那套判定,只保留栈本身,用来做:
//!
//! 1. **防叠开**:同一种覆盖物不许摞第二个(`window.open_dialog` 是栈,连按两次
//!    Ctrl+, 会摞出两个设置框,下面那个永远关不掉);
//! 2. **全局快捷键让路**:审计里那条「无 overlayStack 快捷键让路」——
//!    覆盖物压着时终端类动作一律不派发,只放行原版白名单那两条。
//!
//! # 让路口径逐条对照 `useGlobalHotkeys`
//!
//! ```text
//! if (overlayOpen && id !== 'openSettings' && id !== 'globalSearch') return;
//! ```
//!
//! - `openSettings` 放行:设置面板本身就是弹窗,不放行就没法用键盘开;
//! - `globalSearch` 放行:它是 toggle,弹窗开着时按第二次才关得掉。
//!
//! **终端内查找条不算「挡路的覆盖物」**:原版压根没把它放进 overlayStack —— 挡住
//! 全局快捷键的是 `isTypingTarget`(焦点在输入框里),焦点一离开查找条,
//! Ctrl+Shift+T 之类照常生效。这里照此口径:查找条进栈(为了防叠开与栈顶查询),
//! 但 [`blocks_hotkeys`] 对它返回 false。
//!
//! # 为什么是 `thread_local` 而不是 gpui `Global`
//!
//! 与 [`crate::ui`] 的配色表同一个理由:gpui 的视图全在主线程上跑,一份
//! `thread_local` 足够,而且**取用处不必带 `&mut App`** —— `TerminalPane::drop`
//! 要摘掉自己那条查找条的登记,那里根本拿不到 `cx`。副作用是单测天然隔离
//! (cargo test 一线程一份栈),不必为并发去抢锁。

use std::cell::RefCell;

/// 覆盖物种类标识。一个常量 = 一种「同时只能开一个」的覆盖物。
///
/// 写字面量容易打错,而打错的后果是守卫静默失效(叠开两个一模一样的弹窗),
/// 所以调用点一律走这里的常量。
pub mod kind {
    pub const SETTINGS: &str = "settings";
    pub const ADD_PROJECT: &str = "add-project";
    pub const RENAME_PANE: &str = "rename-pane";
    pub const REMOVE_PROJECT: &str = "remove-project";
    /// 通用输入框(重命名 / 新建文件 / 编辑描述…)。
    pub const PROMPT: &str = "prompt";
    /// 通用确认框(关终端 / 删文件…)。
    pub const CONFIRM: &str = "confirm";
    /// 通用提示框(操作失败)。
    pub const ALERT: &str = "alert";
    /// 右键菜单(同时只可能有一个,`menu::show` 自带「先关上一个」)。
    pub const MENU: &str = "menu";
    /// 全局搜索(Ctrl+Shift+F)。
    pub const GLOBAL_SEARCH: &str = "global-search";
    /// 项目快速切换器(Ctrl+Shift+P)。
    pub const PROJECT_SWITCHER: &str = "project-switcher";
    /// 终端内查找条(Ctrl+F)。**逐 pane 一条**,slot 存 `pty_id`。
    pub const TERMINAL_SEARCH: &str = "terminal-search";
    /// AI 任务标记浮层(tab 栏上那个 ⚑ 按钮弹出来的列表)。
    ///
    /// **原版没把它放进 `overlayStack`**(`PaneGroup.tsx:279-308` 是自己挂 document
    /// 的 mousedown,连 Esc 都没有)。这里登记一条,理由与查找条并进来那次一样:
    /// 不登记的话「浮层开着时按 Ctrl+Shift+F」会同时开两层而浮层无人关闭。
    /// 登记之后 Esc 关闭是 GPUI 结构性免费的(按键沿焦点链派发)—— 比原版多一条
    /// 关闭路,记为改善。
    pub const MARKER_LIST: &str = "marker-list";
    /// 工作区 / 暂存区的单文件 diff(`DiffModal`)。
    pub const GIT_DIFF: &str = "git-diff";
    /// 某次 commit 的多文件 diff(`CommitDiffModal`)。
    pub const GIT_COMMIT_DIFF: &str = "git-commit-diff";
    /// worktree 管理弹窗。
    pub const GIT_WORKTREE: &str = "git-worktree";
    /// worktree 删除确认。**与 [`GIT_WORKTREE`] 不同种类**,所以能叠在它之上
    /// (原版就是嵌套弹窗,`GitWorktreeModal.tsx:779` 的「Esc 归栈顶,不会误关外层」)。
    pub const GIT_WORKTREE_REMOVE: &str = "git-worktree-remove";
    /// 「移动端」面板(中转地址 / 配对二维码 / AI 启动器)。原版没有防叠开,
    /// 是 audit 记的缺口;重置配对的确认框是**另一种类**,照样能叠在它之上。
    pub const MOBILE_RELAY: &str = "mobile-relay";
    /// 「SSH 连接」面板(连接与分组的 CRUD)。删除连接的确认框是**另一种类**
    /// (`CONFIRM`),叠在它之上,Esc 只关确认框 —— 与原版 `overlayStack` 同语义。
    pub const SSH_PANEL: &str = "ssh-panel";
    /// 「关联 SSH」弹窗(项目右键菜单)。与 [`SSH_PANEL`] **不同种类**:
    /// 两个都开着是合法的(在关联弹窗里发现连接名不对,回头去改)。
    pub const SSH_ASSOC: &str = "ssh-assoc";
    /// 「添加远程项目」弹窗。三个入口(项目列表底部 SSH 钮 / 分组右键 /
    /// 首启引导第二颗按钮)共用这一种类,防的正是「两处入口各开一个」。
    pub const ADD_REMOTE_PROJECT: &str = "add-remote-project";
    /// 文件上传/下载发现同名目标后的三选一冲突策略弹窗。
    pub const FILE_CONFLICT: &str = "file-conflict";
    /// 添加远程项目时叠在表单上方的远程目录浏览器。
    pub const REMOTE_DIRECTORY_PICKER: &str = "remote-directory-picker";
    /// 项目环境变量弹窗。
    pub const PROJECT_ENV_VARS: &str = "project-env-vars";
    /// 日期选择浮层(用量面板的自定义起止)。种类唯一 —— 起、止两个输入框各点一次
    /// 只该开一个:第二次点会顶掉第一个(宿主换掉实体,旧的 drop 时摘栈)。
    pub const DATE_PICKER: &str = "date-picker";
}

/// 栈里的一条。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OverlayKey {
    pub kind: &'static str,
    /// 同种类里的第几个。种类唯一的覆盖物填 0;查找条填 `pty_id` ——
    /// 它是**逐 pane** 的,两个分屏各开一条是合法的。
    pub slot: u64,
}

/// 种类唯一的覆盖物(绝大多数)。
pub fn key(kind: &'static str) -> OverlayKey {
    OverlayKey { kind, slot: 0 }
}

/// 某个 pane 的终端查找条。
pub fn terminal_search(pty_id: u32) -> OverlayKey {
    OverlayKey {
        kind: kind::TERMINAL_SEARCH,
        slot: pty_id as u64,
    }
}

/// 这种覆盖物压着时,挡不挡全局快捷键。
///
/// 两类不挡:终端查找条(见模块注释里对 `isTypingTarget` 的说明)与 AI 任务标记
/// 浮层。后者的理由一样 —— 原版这两件都**没进** `overlayStack`,它们进栈只是为了
/// 防叠开与「栈顶是谁」有唯一真相;挡住全局快捷键会凭空多出一条原版没有的限制
/// (标记浮层开着时按 Ctrl+Shift+↑ 该照跳,原版就是这么走的)。
pub fn blocks_hotkeys(kind: &str) -> bool {
    kind != self::kind::TERMINAL_SEARCH && kind != self::kind::MARKER_LIST
}

/// 覆盖物压着时,这个动作还派不派发。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Yield {
    /// 让路(终端类动作 —— 用户正在填表,Ctrl+Shift+W 不该去关终端)。
    ToOverlay,
    /// 不让路。只有 `openSettings` 与 `globalSearch` 两条,照抄原版白名单。
    Never,
}

/// 覆盖物栈本体。纯数据结构,单测直接构造。
#[derive(Default, Debug)]
pub struct Stack {
    entries: Vec<OverlayKey>,
}

impl Stack {
    /// 压入一层。**已经在栈里就返回 `false` 且不重复压** —— 这就是防叠开。
    pub fn push(&mut self, key: OverlayKey) -> bool {
        if self.entries.contains(&key) {
            return false;
        }
        self.entries.push(key);
        true
    }

    /// 弹出指定覆盖物。幂等,且**不要求是栈顶** ——
    /// 异常关闭顺序(先关底下那层)也不会把栈卡死,与原版 `popOverlay(id)` 同语义。
    pub fn pop(&mut self, key: OverlayKey) {
        if let Some(idx) = self.entries.iter().position(|e| *e == key) {
            self.entries.remove(idx);
        }
    }

    pub fn contains(&self, key: OverlayKey) -> bool {
        self.entries.contains(&key)
    }

    /// 栈顶(最后压进来的那个)。空栈返回 `None`。
    pub fn top(&self) -> Option<OverlayKey> {
        self.entries.last().copied()
    }

    pub fn is_top(&self, key: OverlayKey) -> bool {
        self.top() == Some(key)
    }

    /// 有没有「会挡住全局快捷键」的覆盖物压着。
    pub fn blocking(&self) -> bool {
        self.entries.iter().any(|e| blocks_hotkeys(e.kind))
    }

    /// 这个动作现在派不派发。
    pub fn allows(&self, yielding: Yield) -> bool {
        yielding == Yield::Never || !self.blocking()
    }
}

thread_local! {
    /// 当前进程(主线程)的那一份栈。
    static CURRENT: RefCell<Stack> = RefCell::new(Stack::default());
}

/// 压入一层。返回 `false` = 同一层已经开着(调用方应当直接放弃这次打开)。
pub fn push(key: OverlayKey) -> bool {
    CURRENT.with(|s| s.borrow_mut().push(key))
}

/// 弹出一层(幂等)。
pub fn pop(key: OverlayKey) {
    CURRENT.with(|s| s.borrow_mut().pop(key));
}

/// 这一层现在开着吗。
pub fn contains(key: OverlayKey) -> bool {
    CURRENT.with(|s| s.borrow().contains(key))
}

/// 这一层是栈顶吗(主动关闭时用:只关最上面那个,别越过别人去关底下的)。
pub fn is_top(key: OverlayKey) -> bool {
    CURRENT.with(|s| s.borrow().is_top(key))
}

/// 全局快捷键让路判据。`false` = 这次按键该让给覆盖物。
pub fn allows(yielding: Yield) -> bool {
    CURRENT.with(|s| s.borrow().allows(yielding))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 同种类不许摞第二个;摘掉之后又能开。
    #[test]
    fn 同种类防叠开() {
        let mut stack = Stack::default();
        assert!(stack.push(key(kind::SETTINGS)));
        assert!(!stack.push(key(kind::SETTINGS)), "第二次必须被拦下");
        stack.pop(key(kind::SETTINGS));
        assert!(stack.push(key(kind::SETTINGS)), "关掉之后要能重新开");
    }

    /// 不同种类照样能叠(设置框里再弹确认框是合法的)。
    #[test]
    fn 不同种类可以叠() {
        let mut stack = Stack::default();
        assert!(stack.push(key(kind::SETTINGS)));
        assert!(stack.push(key(kind::CONFIRM)));
        assert_eq!(stack.top(), Some(key(kind::CONFIRM)), "后进的在栈顶");
        assert!(stack.is_top(key(kind::CONFIRM)));
        assert!(!stack.is_top(key(kind::SETTINGS)));
    }

    /// 查找条是逐 pane 的:两个分屏各开一条互不相干。
    #[test]
    fn 查找条按_pane_各算一条() {
        let mut stack = Stack::default();
        assert!(stack.push(terminal_search(7)));
        assert!(stack.push(terminal_search(8)));
        assert!(!stack.push(terminal_search(7)), "同一个 pane 不许开两条");
        stack.pop(terminal_search(7));
        assert!(!stack.contains(terminal_search(7)));
        assert!(stack.contains(terminal_search(8)), "别把邻居的也摘了");
    }

    /// 弹出**不要求是栈顶**:异常关闭顺序不该把栈卡死。
    #[test]
    fn 非栈顶也能摘掉() {
        let mut stack = Stack::default();
        stack.push(key(kind::SETTINGS));
        stack.push(key(kind::CONFIRM));
        stack.pop(key(kind::SETTINGS));
        assert!(!stack.contains(key(kind::SETTINGS)));
        assert_eq!(stack.top(), Some(key(kind::CONFIRM)));
        // 摘不存在的条目是空操作,不 panic
        stack.pop(key(kind::ALERT));
        assert_eq!(stack.top(), Some(key(kind::CONFIRM)));
    }

    /// 覆盖物压着 → 终端类动作让路;白名单那两条照常;关掉后全部恢复。
    #[test]
    fn 弹窗压着时全局动作让路() {
        let mut stack = Stack::default();
        assert!(stack.allows(Yield::ToOverlay), "空栈时什么都放行");

        stack.push(key(kind::SETTINGS));
        assert!(!stack.allows(Yield::ToOverlay), "弹窗开着必须让路");
        assert!(stack.allows(Yield::Never), "openSettings / globalSearch 不让路");

        stack.pop(key(kind::SETTINGS));
        assert!(stack.allows(Yield::ToOverlay), "关掉之后恢复");
    }

    /// 右键菜单同样挡路(原版把 'menu' 也压进 overlayStack)。
    #[test]
    fn 右键菜单也挡路() {
        let mut stack = Stack::default();
        stack.push(key(kind::MENU));
        assert!(!stack.allows(Yield::ToOverlay));
    }

    /// 终端查找条**不挡**全局快捷键 —— 原版靠 isTypingTarget 而不是 overlayStack,
    /// 焦点离开查找条之后 Ctrl+Shift+T 该照常新建终端。
    #[test]
    fn 查找条不挡全局快捷键() {
        let mut stack = Stack::default();
        stack.push(terminal_search(3));
        assert!(stack.blocking() == false);
        assert!(stack.allows(Yield::ToOverlay));
        // 但只要上面再压一个真弹窗,照样让路
        stack.push(key(kind::CONFIRM));
        assert!(!stack.allows(Yield::ToOverlay));
    }

    /// AI 任务标记浮层同样**不挡**全局快捷键 —— 原版压根没把它放进 overlayStack,
    /// 进栈只为防叠开与 Esc;挡住就等于凭空多一条原版没有的限制。
    #[test]
    fn 标记浮层不挡全局快捷键() {
        let mut stack = Stack::default();
        stack.push(key(kind::MARKER_LIST));
        assert!(!stack.blocking());
        assert!(stack.allows(Yield::ToOverlay));
        // 防叠开照旧生效
        assert!(!stack.push(key(kind::MARKER_LIST)));
        // 上面再压一个真弹窗就照样让路
        stack.push(key(kind::SETTINGS));
        assert!(!stack.allows(Yield::ToOverlay));
    }

    /// 进程级那一份栈的读写口径与 [`Stack`] 一致(线程本地,用例之间不互踩)。
    #[test]
    fn 全局入口与栈本体同语义() {
        assert!(push(key(kind::ALERT)));
        assert!(!push(key(kind::ALERT)));
        assert!(contains(key(kind::ALERT)));
        assert!(is_top(key(kind::ALERT)));
        assert!(!allows(Yield::ToOverlay));
        assert!(allows(Yield::Never));
        pop(key(kind::ALERT));
        assert!(!contains(key(kind::ALERT)));
        assert!(allows(Yield::ToOverlay));
    }
}
