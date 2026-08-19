//! 设置面板:两级侧栏 + 10 个分页。逐页对照 `src/components/SettingsModal.tsx`。
//!
//! ```text
//! ┌──────────────┬────────────────────────────────────────┐
//! │ 终端          │                                        │
//! │  · Shell      │   (当前分页)                            │
//! │  · 复制粘贴    │                                        │
//! │ 外观          │                                        │
//! │  · 主题与语言  │                                        │
//! │  · 字体       │                                        │
//! │ …            │                                        │
//! └──────────────┴────────────────────────────────────────┘
//! ```
//!
//! # 三条贯穿全文件的约定
//!
//! 1. **没有「保存」按钮**:每一项都是即时生效 + 即时落盘(500ms 防抖,
//!    `AppStore::save_config_soon`)。通用写入口是 [`AppStore::patch_config`],
//!    需要额外副作用的(主题 / 字号 / 字族 / 回滚行数 / 停留时长)各有专用 setter;
//! 2. **数字行是草稿态**:输入期间只改草稿,失焦 / 回车才归一并提交 ——
//!    边打字边 clamp 会让「1000」在敲到「1」时就被吃掉
//!    (`SettingsModal.tsx:167-171` 的注释)。滑块相反,**拖动即时提交**;
//! 3. **分页 id 一字不改**:[`SettingsPage::id`] 返回的字符串与原版
//!    `SettingsPage` 联合类型完全一致,深链(`initial_page`)不会因为重排失效。
//!
//! # 通用原语在哪
//!
//! Toggle / SettingRow / ChoiceGroup / 滑块 / 键帽全部**自绘**在 [`crate::ui`]
//! (不用 `gpui_component` 的 `switch` 与 `setting`,理由见那边的注释)。
//!
//! # 「UI 有、底层没有」的两项
//!
//! 内置皮肤 blueprint / fluent2(`theme.rs` 按 `none` 处理)与终端连体字
//! (自绘渲染器按「一个字符一列」摆放)在 GPUI 侧都还没有。**照原版画出来但置灰**,
//! 各配一句说明 —— 不做成「看着能点、点了没反应」。
//!
//! # 无消费方的设置项
//!
//! 长文本粘贴三项、远程粘贴目录、托盘三项、智能 Ctrl+C/V —— 字段都已在磁盘格式里,
//! UI 照原版做出来,但 GPUI 侧还没有消费方(分别属 audit #30 / #28 / #21 与
//! 终端剪贴板批)。改了会落盘、重启后还在,只是暂时没有效果。

use std::path::PathBuf;

use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, Hsla, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, PathPromptOptions, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Task, Window, div, img,
    prelude::FluentBuilder, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use mt_ai::hook_registry::{self, HookAgent, HookRegistrationInfo};
use mt_config::{EditorConfig, ShellConfig};
use mt_ui::theme_bridge::{ThemeSlot, resolve_theme_pack};

use crate::hotkeys;
use crate::i18n::{Locale, t, tr};
use crate::prompt::{Confirm, kind, open_guarded, show_alert};
use crate::shell_ops::{parse_args, valid_shell};
use crate::store::{AppStore, MAX_SCROLLBACK, resolve_scrollback};
use crate::ui;

// ─── 分页 ─────────────────────────────────────────────────────

/// 设置分页。
///
/// ⚠️ [`Self::id`] 的字符串**与原版一字不差**(`SettingsModal.tsx:40-50`)——
/// 原版注释明说「旧 id 一律保留、拆页只挪内容不改 key」,因为外部深链
/// (`initialPage`)会因为改名失效。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsPage {
    Terminal,
    Clipboard,
    Appearance,
    Font,
    AiNotification,
    AiHook,
    System,
    Editor,
    Shortcuts,
    About,
}

impl SettingsPage {
    /// 深链 id。与原版联合类型的字面量逐条对齐。
    pub fn id(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Clipboard => "clipboard",
            Self::Appearance => "appearance",
            Self::Font => "font",
            Self::AiNotification => "ai-notification",
            Self::AiHook => "ai-hook",
            Self::System => "system",
            Self::Editor => "editor",
            Self::Shortcuts => "shortcuts",
            Self::About => "about",
        }
    }

    /// 侧栏标签的 i18n key(`settings` 命名空间内的相对 key)。
    fn label_key(self) -> &'static str {
        match self {
            Self::Terminal => "menu.shell",
            Self::Clipboard => "menu.clipboard",
            Self::Appearance => "menu.appearance",
            Self::Font => "menu.font",
            Self::AiNotification => "menu.aiNotification",
            Self::AiHook => "menu.aiHook",
            Self::System => "menu.general",
            Self::Editor => "menu.editor",
            Self::Shortcuts => "menu.shortcuts",
            Self::About => "menu.about",
        }
    }

    /// 深链入口的解析口(`initial_page`)。原版那两处入口都传 `undefined`,
    /// 这个口子同样先留着。
    #[allow(dead_code)]
    pub fn from_id(id: &str) -> Option<Self> {
        ALL_PAGES.iter().copied().find(|p| p.id() == id)
    }
}

/// 侧栏分组。空标题 = 一条分隔线(`SettingsModal.tsx:2059-2065`)。
const MENU_GROUPS: &[(&str, &[SettingsPage])] = &[
    (
        "menu.groupTerminal",
        &[SettingsPage::Terminal, SettingsPage::Clipboard],
    ),
    (
        "menu.groupAppearance",
        &[SettingsPage::Appearance, SettingsPage::Font],
    ),
    (
        "menu.groupAi",
        &[SettingsPage::AiNotification, SettingsPage::AiHook],
    ),
    (
        "menu.groupSystem",
        &[SettingsPage::System, SettingsPage::Editor],
    ),
    ("", &[SettingsPage::Shortcuts, SettingsPage::About]),
];

/// 扁平化后的分页序列 —— ↑↓ 在它上面环形移动,跳过分组标题
/// (`SettingsModal.tsx:2069` 的 `MENU_ITEMS`)。
pub const ALL_PAGES: &[SettingsPage] = &[
    SettingsPage::Terminal,
    SettingsPage::Clipboard,
    SettingsPage::Appearance,
    SettingsPage::Font,
    SettingsPage::AiNotification,
    SettingsPage::AiHook,
    SettingsPage::System,
    SettingsPage::Editor,
    SettingsPage::Shortcuts,
    SettingsPage::About,
];

// ─── 数字行的归一(纯函数,可测) ────────────────────────────────

/// 哪一个数字设置项。每项自带取值范围,归一规则见 [`normalize_number`]。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NumField {
    /// 回滚行数(`terminalScrollback`)
    Scrollback,
    /// 拖选停留自动复制时长(`selectionAutoCopySecs`),**浮点且有自定义规则**
    Dwell,
    /// 长文本粘贴的行数阈值
    LineThreshold,
    /// 长文本粘贴的字符数阈值
    CharThreshold,
    /// 托盘菜单最多显示的项目数
    TrayMax,
}

impl NumField {
    /// `(min, max)`。原版的默认归一规则是 `v >= (min ?? 0)` 才收,再 `min(v, max)`。
    fn bounds(self) -> (f64, f64) {
        match self {
            Self::Scrollback => (0.0, MAX_SCROLLBACK as f64),
            Self::Dwell => (0.2, 60.0),
            Self::LineThreshold => (0.0, 100_000.0),
            Self::CharThreshold => (0.0, 10_000_000.0),
            Self::TrayMax => (1.0, 20.0),
        }
    }
}

/// 数字设置行的归一(`SettingsModal.tsx:200-209` 的 `commit`)。
///
/// 返回 `None` = 这次输入无效,调用方**回落已保存值**(而不是写 0)。
///
/// - 默认规则:`finite && v >= min` → `min(v, max)`;整数项截尾(等价 `parseInt`);
/// - `Dwell` 有自定义规则(`SettingsModal.tsx:681-684`):
///   **`0` 是「关掉」的唯一出口** —— 静默覆盖剪贴板的行为必须可退出;
///   负数 / 非数字回落,其余一律钳在 `0.2..=60`。
///
/// 与原版的一处口径差:`"1000abc"` 在 JS 里 `parseInt` 得 1000,这里
/// `parse::<f64>()` 直接失败 → 回落已保存值。宁可不动也不猜。
pub fn normalize_number(field: NumField, draft: &str) -> Option<f64> {
    let raw: f64 = draft.trim().parse().ok()?;
    if !raw.is_finite() {
        return None;
    }
    let (min, max) = field.bounds();
    match field {
        NumField::Dwell => {
            if raw < 0.0 {
                None
            } else if raw == 0.0 {
                Some(0.0)
            } else {
                Some(raw.clamp(min, max))
            }
        }
        _ => (raw >= min).then(|| raw.trunc().min(max)),
    }
}

/// 数字的显示串。整数项不带小数点(与 `<input type=number>` 的回显一致)。
fn number_text(field: NumField, value: f64) -> String {
    match field {
        NumField::Dwell if value.fract() != 0.0 => format!("{value}"),
        _ => format!("{value:.0}"),
    }
}

// ─── 文本行 ───────────────────────────────────────────────────

/// 哪一个文本设置项(草稿态,失焦 / 回车提交)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TextField {
    RemotePasteDir,
    UiFontFamily,
    TerminalFontFamily,
}

/// 远程粘贴目录的归一(`SettingsModal.tsx:657-663`)。
///
/// `trim()` 后为空 → 回落默认值(**不落空串**让后端每次兜底)。
/// `..` 的拒绝在后端 `resolve_paste_dir`,前端不重复判 —— 两处判定会漂移。
pub fn normalize_remote_paste_dir(draft: &str) -> String {
    match draft.trim() {
        "" => mt_config::default_remote_paste_dir(),
        text => text.to_string(),
    }
}

// ─── 三字段联动(外观页,纯函数) ──────────────────────────────

/// 主题 / 皮肤单选段的当前选中值。
///
/// **激活外置皮肤时返回空串** —— 三个按钮全不高亮(`SettingsModal.tsx:827`
/// 的 `config.customThemeId ? '' : config.theme`)。这不是 bug:外置皮肤既不是
/// dark/light/auto 里的任何一个,也不是 none/blueprint/fluent2 里的任何一个。
pub fn choice_value<'a>(custom_theme_id: Option<&str>, value: &'a str) -> &'a str {
    if custom_theme_id.is_some() { "" } else { value }
}

// ─── hook 页的默认勾选(纯函数) ───────────────────────────────

/// 第一次拿到注册现状时的默认勾选(`SettingsModal.tsx:1017-1025`)。
///
/// 默认勾「已经装了的那几家」——用户再点一次注册就是补齐新事件,不会顺手往
/// 没在用的 CLI 里写配置;**一家都没装过(首次使用)才全选**,保住「一键注册」体验。
pub fn default_selected_agents(list: &[HookRegistrationInfo]) -> Vec<String> {
    let installed: Vec<String> = list
        .iter()
        .filter(|r| r.registered > 0)
        .map(|r| r.agent.clone())
        .collect();
    if installed.is_empty() {
        list.iter().map(|r| r.agent.clone()).collect()
    } else {
        installed
    }
}

// ─── system 页:托盘子项的显隐(纯函数) ───────────────────────

/// 托盘的两个从属项要不要**渲染**。
///
/// ⚠️ 与 clipboard 页的「置灰」处理**不一样**:原版这里是
/// `{trayEnabled && (<>...</>)}`(`SettingsModal.tsx:1368-1385`),总开关关掉时
/// 两行整个不出现,而不是灰着还占位。别抄串了。
pub fn tray_children_visible(tray_enabled: bool) -> bool {
    tray_enabled
}

// ─── 版本比较(about 页,纯函数) ──────────────────────────────

/// 语义版本比较(`src/utils/updateChecker.ts:11-19`):
/// 去掉前导 `v`,按 `.` 分段数值比较,缺段按 0。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parts = |s: &str| -> Vec<u64> {
        s.trim()
            .trim_start_matches(['v', 'V'])
            .split('.')
            .map(|seg| {
                seg.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    };
    let (a, b) = (parts(a), parts(b));
    for i in 0..a.len().max(b.len()) {
        let l = a.get(i).copied().unwrap_or(0);
        let r = b.get(i).copied().unwrap_or(0);
        if l != r {
            return l.cmp(&r);
        }
    }
    std::cmp::Ordering::Equal
}

/// ISO 时间戳 → `2026/8/19`。
///
/// 原版是 `new Date(publishedAt).toLocaleDateString('zh-CN')`——**locale 写死**
/// (`SettingsModal.tsx:1635`),所以这里也不跟界面语言走。
pub fn format_published_at(iso: &str) -> String {
    let date = iso.split('T').next().unwrap_or(iso);
    let mut parts = date.split('-');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(y), Some(m), Some(d)) if y.len() == 4 && !m.is_empty() && !d.is_empty() => format!(
            "{y}/{}/{}",
            m.trim_start_matches('0'),
            d.trim_start_matches('0')
        ),
        _ => iso.to_string(),
    }
}

/// GitHub 上的仓库(`updateChecker.ts:3`)。
const GITHUB_REPO: &str = "dreamlonglll/mini-term";

/// 一条 release 的关键信息。
#[derive(Clone, Debug)]
pub struct ReleaseInfo {
    pub version: String,
    pub url: String,
    pub published_at: String,
}

/// 拉最新 release。**整体阻塞**,调用方一律丢 `cx.background_executor()`。
///
/// 复用 `pricing` 那份 `zed-reqwest`(blocking feature 已开,净新增 crate = 0)。
/// GitHub API 强制要求 `User-Agent`,缺了直接 403。
fn fetch_latest_release() -> Result<ReleaseInfo, String> {
    let url = format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest");
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(url)
        .header("User-Agent", "mini-term")
        .header("Accept", "application/vnd.github+json")
        .send()
        // 传输层失败(断网 / DNS / TLS)的原文是英文且没什么可操作性,
        // 与原版一样收成一句「检查失败,请稍后重试」
        .map_err(|e| {
            eprintln!("[settings] 检查更新失败: {e}");
            t("settings", "about.checkFailed").to_string()
        })?;
    let status = resp.status().as_u16();
    if status == 404 {
        return Err(t("updateChecker", "noRelease").to_string());
    }
    if !resp.status().is_success() {
        return Err(tr!("updateChecker", "requestFailed", status = status));
    }
    let text = resp.text().map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let field = |name: &str| {
        json.get(name)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Ok(ReleaseInfo {
        version: field("tag_name"),
        url: field("html_url"),
        published_at: field("published_at"),
    })
}

// ─── 外置皮肤卡片的数据 ───────────────────────────────────────

/// 一张皮肤卡片要画的东西(刷新列表时算一次,不每帧重解析色值)。
struct ThemeCard {
    theme_id: String,
    name: String,
    background: Hsla,
    panel: Hsla,
    accent: Hsla,
    text: Hsla,
    /// 背景图绝对路径(包里没声明 / 文件不在盘上 = `None`)。
    image: Option<PathBuf>,
}

// ─── 面板视图 ─────────────────────────────────────────────────

/// 正在编辑的一行(shell 列表 / 编辑器列表共用这个形状)。
///
/// `None` = 表单没打开;`Some(None)` = 新增;`Some(Some(i))` = 编辑第 i 行。
type Editing = Option<Option<usize>>;

pub struct SettingsView {
    store: Entity<AppStore>,
    page: SettingsPage,
    focus: FocusHandle,

    // ── terminal 页:shell 列表 ──
    shell_editing: Editing,
    shell_name: Entity<InputState>,
    shell_command: Entity<InputState>,
    shell_args: Entity<InputState>,
    shell_error: Option<&'static str>,

    // ── editor 页:编辑器列表 ──
    editor_editing: Editing,
    editor_name: Entity<InputState>,
    editor_command: Entity<InputState>,

    // ── 数字行(草稿态)──
    num_scrollback: Entity<InputState>,
    num_dwell: Entity<InputState>,
    num_line_threshold: Entity<InputState>,
    num_char_threshold: Entity<InputState>,
    num_tray_max: Entity<InputState>,

    // ── 文本行(草稿态)──
    txt_remote_paste_dir: Entity<InputState>,
    txt_ui_font: Entity<InputState>,
    txt_terminal_font: Entity<InputState>,

    // ── appearance 页:外置皮肤 ──
    theme_cards: Vec<ThemeCard>,
    theme_error: Option<String>,
    /// 成功提示(生成示例皮肤);与 `theme_error` 互斥展示。
    theme_notice: Option<String>,

    // ── ai-hook 页 ──
    hook_running: bool,
    hook_port: u16,
    registrations: Vec<HookRegistrationInfo>,
    /// 本次注册/卸载作用于哪几家;`None` = 还没按注册现状初始化过。
    selected_agents: Option<Vec<String>>,
    hook_busy: bool,
    hook_result: String,
    snippet: Option<serde_json::Value>,
    show_snippet: bool,
    snippet_tab: &'static str,

    // ── ai-notification 页 ──
    /// 选到非 `.wav` 时的提示(`notify.rs` 只认 wav,其余静默回落系统提示音)。
    sound_warning: bool,

    // ── about 页 ──
    checking: bool,
    latest: Option<ReleaseInfo>,
    update_error: Option<String>,

    /// 后台任务(hook 动作 / 皮肤导入 / 检查更新)。换一次就丢掉上一次。
    _job: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

/// 打开设置面板。`initial_page` 是深链入口(原版留的口子,目前恒传 `None`)。
pub fn open_settings(
    store: Entity<AppStore>,
    initial_page: Option<SettingsPage>,
    window: &mut Window,
    cx: &mut App,
) {
    // 守卫要在**建视图之前**判一次:`open_guarded` 拦下来的时候,下面这一堆
    // 输入框已经建好了,而它们永远不会被画出来(与 `show_prompt` 同一个坑)。
    if crate::overlay::contains(crate::overlay::key(kind::SETTINGS)) {
        return;
    }
    let view = cx.new(|cx| SettingsView::new(store, initial_page, window, cx));
    let focus = view.read(cx).focus.clone();

    open_guarded(kind::SETTINGS, window, cx, {
        let view = view.clone();
        move |dialog, window, _cx| {
            // 原版 `w-[680px] max-h-[80vh]`
            let height = (window.viewport_size().height * 0.8).min(px(640.0));
            dialog
                .title(t("settings", "title"))
                .w(px(680.0))
                .p_0()
                // 改了半天设置、误点遮罩就没了 —— 面板里还有编辑中的表单
                .overlay_closable(false)
                .child(div().h(height).child(view.clone()))
        }
    });

    // Dialog 打开时会把焦点抢到自己面板上,↑↓ 导航要的焦点必须排在它后面
    window.defer(cx, move |window, _cx| {
        window.focus(&focus);
    });
}

impl SettingsView {
    fn new(
        store: Entity<AppStore>,
        initial_page: Option<SettingsPage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = store.read(cx).config().clone();

        let num = |cx: &mut Context<Self>, window: &mut Window, field: NumField, value: f64| {
            cx.new(|cx| InputState::new(window, cx).default_value(number_text(field, value)))
        };
        let ph = |cx: &mut Context<Self>, window: &mut Window, text: &'static str| {
            cx.new(|cx| InputState::new(window, cx).placeholder(text))
        };

        let num_scrollback = num(
            cx,
            window,
            NumField::Scrollback,
            resolve_scrollback(config.terminal_scrollback as f64) as f64,
        );
        let num_dwell = num(
            cx,
            window,
            NumField::Dwell,
            config.selection_auto_copy_secs.unwrap_or(1.0),
        );
        let num_line_threshold = num(
            cx,
            window,
            NumField::LineThreshold,
            config.long_paste_line_threshold as f64,
        );
        let num_char_threshold = num(
            cx,
            window,
            NumField::CharThreshold,
            config.long_paste_char_threshold as f64,
        );
        let num_tray_max = num(
            cx,
            window,
            NumField::TrayMax,
            config.tray_max_projects.unwrap_or(5) as f64,
        );

        let txt_remote_paste_dir = cx.new(|cx| {
            InputState::new(window, cx)
                // placeholder 就是默认值本身(`SettingsModal.tsx:728`)
                .placeholder(mt_config::default_remote_paste_dir())
                .default_value(config.remote_paste_dir.clone())
        });
        let txt_ui_font = cx.new(|cx| {
            InputState::new(window, cx)
                // 原版 placeholder 就是这串字面量(`SettingsModal.tsx:913`)
                .placeholder("'DM Sans', system-ui, sans-serif")
                .default_value(config.ui_font_family.clone().unwrap_or_default())
        });
        let txt_terminal_font = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(DEFAULT_TERMINAL_FONT_PLACEHOLDER)
                .default_value(config.terminal_font_family.clone().unwrap_or_default())
        });

        let mut this = Self {
            store,
            page: initial_page.unwrap_or(SettingsPage::Terminal),
            focus: cx.focus_handle(),
            shell_editing: None,
            shell_name: ph(cx, window, t("settings", "terminal.newNamePlaceholder")),
            shell_command: ph(cx, window, t("settings", "terminal.newCommandPlaceholder")),
            shell_args: ph(cx, window, t("settings", "terminal.newArgsPlaceholder")),
            shell_error: None,
            editor_editing: None,
            editor_name: ph(cx, window, t("settings", "editor.newEditorNamePlaceholder")),
            editor_command: ph(
                cx,
                window,
                t("settings", "editor.newEditorCommandPlaceholder"),
            ),
            num_scrollback,
            num_dwell,
            num_line_threshold,
            num_char_threshold,
            num_tray_max,
            txt_remote_paste_dir,
            txt_ui_font,
            txt_terminal_font,
            theme_cards: Vec::new(),
            theme_error: None,
            theme_notice: None,
            hook_running: false,
            hook_port: 0,
            registrations: Vec::new(),
            selected_agents: None,
            hook_busy: false,
            hook_result: String::new(),
            snippet: None,
            show_snippet: false,
            snippet_tab: "claude",
            sound_warning: false,
            checking: false,
            latest: None,
            update_error: None,
            _job: None,
            _subs: Vec::new(),
        };

        // 草稿行:失焦 / 回车才归一并提交(见模块注释第 2 条)。
        // 走 `subscribe_in` 而不是 `subscribe` —— 归一后要把值写回输入框,
        // 而 `InputState::set_value` 要 `&mut Window`。
        let numeric = [
            (this.num_scrollback.clone(), NumField::Scrollback),
            (this.num_dwell.clone(), NumField::Dwell),
            (this.num_line_threshold.clone(), NumField::LineThreshold),
            (this.num_char_threshold.clone(), NumField::CharThreshold),
            (this.num_tray_max.clone(), NumField::TrayMax),
        ];
        for (entity, field) in numeric {
            this._subs.push(cx.subscribe_in(
                &entity.clone(),
                window,
                move |this: &mut Self, input, event: &InputEvent, window, cx| {
                    if commits(event) {
                        this.commit_number(field, input, window, cx);
                    }
                },
            ));
        }
        let texts = [
            (this.txt_remote_paste_dir.clone(), TextField::RemotePasteDir),
            (this.txt_ui_font.clone(), TextField::UiFontFamily),
            (this.txt_terminal_font.clone(), TextField::TerminalFontFamily),
        ];
        for (entity, field) in texts {
            this._subs.push(cx.subscribe_in(
                &entity.clone(),
                window,
                move |this: &mut Self, input, event: &InputEvent, window, cx| {
                    if commits(event) {
                        this.commit_text(field, input, window, cx);
                    }
                },
            ));
        }

        this.refresh_theme_packs(cx);
        this.refresh_hook_state(cx);
        this
    }

    // ── 提交 ──

    fn commit_number(
        &mut self,
        field: NumField,
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = input.read(cx).value().to_string();
        let saved = self.saved_number(field, cx);
        let next = normalize_number(field, &draft).unwrap_or(saved);
        // 归一后的值写回输入框(原版 `setDraft(String(next))`)
        let text = number_text(field, next);
        if input.read(cx).value().as_ref() != text.as_str() {
            input.update(cx, |state, cx| state.set_value(text, window, cx));
        }
        if (next - saved).abs() < f64::EPSILON {
            return;
        }
        self.store.update(cx, |store, cx| match field {
            NumField::Scrollback => store.set_terminal_scrollback(next as u32, cx),
            NumField::Dwell => store.set_selection_auto_copy_secs(next, cx),
            NumField::LineThreshold => {
                store.patch_config(|c| c.long_paste_line_threshold = next as u32, cx)
            }
            NumField::CharThreshold => {
                store.patch_config(|c| c.long_paste_char_threshold = next as u32, cx)
            }
            NumField::TrayMax => store.patch_config(|c| c.tray_max_projects = Some(next as u32), cx),
        });
    }

    fn saved_number(&self, field: NumField, cx: &App) -> f64 {
        let config = self.store.read(cx).config();
        match field {
            NumField::Scrollback => resolve_scrollback(config.terminal_scrollback as f64) as f64,
            NumField::Dwell => config.selection_auto_copy_secs.unwrap_or(1.0),
            NumField::LineThreshold => config.long_paste_line_threshold as f64,
            NumField::CharThreshold => config.long_paste_char_threshold as f64,
            NumField::TrayMax => config.tray_max_projects.unwrap_or(5) as f64,
        }
    }

    fn commit_text(
        &mut self,
        field: TextField,
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = input.read(cx).value().to_string();
        match field {
            TextField::RemotePasteDir => {
                let next = normalize_remote_paste_dir(&draft);
                if next != draft {
                    input.update(cx, |state, cx| state.set_value(next.clone(), window, cx));
                }
                if self.store.read(cx).config().remote_paste_dir == next {
                    return;
                }
                self.store.update(cx, |store, cx| {
                    store.patch_config(|c| c.remote_paste_dir = next, cx)
                });
            }
            // 空串提交 → 写 `None` 而不是空字符串(`SettingsModal.tsx:881, 920`)
            TextField::UiFontFamily => {
                let next = Some(draft).filter(|s| !s.trim().is_empty());
                self.store
                    .update(cx, |store, cx| store.set_ui_font_family(next, cx));
            }
            TextField::TerminalFontFamily => {
                let next = Some(draft).filter(|s| !s.trim().is_empty());
                self.store
                    .update(cx, |store, cx| store.set_terminal_font_family(next, cx));
            }
        }
    }

    // ── 外置皮肤 ──

    fn refresh_theme_packs(&mut self, cx: &mut Context<Self>) {
        self.theme_error = None;
        self.theme_notice = None;
        self.theme_cards = crate::theme::list_packs()
            .into_iter()
            .map(|(def, dir)| {
                let applied = resolve_theme_pack(&def, Some(&dir));
                ThemeCard {
                    theme_id: def.id.clone(),
                    name: def.name.clone(),
                    background: applied.color(ThemeSlot::Background),
                    panel: applied.color(ThemeSlot::Panel),
                    accent: applied.color(ThemeSlot::Accent),
                    text: applied.color(ThemeSlot::Text),
                    image: def
                        .image
                        .as_deref()
                        .map(|name| dir.join(name))
                        .filter(|p| p.is_file()),
                }
            })
            .collect();
        cx.notify();
    }

    /// 导入皮肤(目录 / zip)与生成示例:三条都要写盘,统一丢后台。
    fn run_theme_job(
        &mut self,
        job: impl FnOnce(mt_config::ThemePacks) -> Result<String, String> + Send + 'static,
        notice: Option<fn(&str) -> String>,
        cx: &mut Context<Self>,
    ) {
        let packs = crate::theme::theme_packs();
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { job(packs) }).await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.refresh_theme_packs(cx);
                match result {
                    Ok(id) => this.theme_notice = notice.map(|f| f(&id)),
                    Err(err) => this.theme_error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    // ── hook 页 ──

    fn refresh_hook_state(&mut self, cx: &mut Context<Self>) {
        let status = self.store.read(cx).ai().hook_status();
        self.hook_running = status.running;
        self.hook_port = status.port;
        // 注册现状要读三家的配置文件 —— 丢后台,回主线程再改状态
        self._job = Some(cx.spawn(async move |this, cx| {
            let list = cx
                .background_executor()
                .spawn(async { hook_registry::get_ai_hook_registrations() })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                if this.selected_agents.is_none() {
                    this.selected_agents = Some(default_selected_agents(&list));
                }
                this.registrations = list;
                cx.notify();
            });
        }));
    }

    /// 当前勾选的注入目标。
    fn agents(&self) -> Vec<String> {
        self.selected_agents.clone().unwrap_or_default()
    }

    fn run_hook_action(&mut self, register: bool, cx: &mut Context<Self>) {
        let agents: Vec<HookAgent> = self
            .agents()
            .iter()
            .filter_map(|a| match a.as_str() {
                "claude" => Some(HookAgent::Claude),
                "codex" => Some(HookAgent::Codex),
                "grok" => Some(HookAgent::Grok),
                _ => None,
            })
            .collect();
        // 空选择由按钮 disabled 挡住;真走到这里也不能放行 ——
        // 后端对空列表会回落成「三家全上」(hook_registry::resolve_targets)
        if agents.is_empty() {
            return;
        }
        self.hook_busy = true;
        self.hook_result.clear();
        cx.notify();
        // 注册要往用户主目录写配置文件(还会复制 hook 二进制),必须丢后台
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    if register {
                        hook_registry::register_ai_hooks(Some(agents))
                    } else {
                        hook_registry::unregister_ai_hooks(Some(agents))
                    }
                })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.hook_busy = false;
                this.hook_result = match result {
                    Ok(msg) => msg,
                    Err(err) => err,
                };
                // 跑完刷一次现状(徽章要变)
                this.refresh_hook_state(cx);
                cx.notify();
            });
        }));
    }

    fn toggle_hook_server(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if self.hook_busy {
            return;
        }
        self.hook_busy = true;
        cx.notify();
        let ai = self.store.read(cx).ai();
        let store = self.store.clone();
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { ai.set_hook_enabled(enabled) })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.hook_busy = false;
                match result {
                    // **成功了才写配置**(原版 handleToggleHook 的同一顺序):
                    // 端口被占时配置不该记成「已开」
                    Ok(()) => store.update(cx, |store, cx| {
                        store.patch_config(|c| c.hook_enabled = enabled, cx)
                    }),
                    Err(err) => this.hook_result = err,
                }
                this.refresh_hook_state(cx);
                cx.notify();
            });
        }));
    }

    fn toggle_snippet(&mut self, cx: &mut Context<Self>) {
        if self.show_snippet {
            self.show_snippet = false;
            cx.notify();
            return;
        }
        self._job = Some(cx.spawn(async move |this, cx| {
            let data = cx
                .background_executor()
                .spawn(async { hook_registry::get_hook_config_snippet() })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.snippet = data.ok();
                this.show_snippet = true;
                cx.notify();
            });
        }));
    }

    // ── about 页 ──

    fn check_update(&mut self, cx: &mut Context<Self>) {
        if self.checking {
            return;
        }
        self.checking = true;
        self.update_error = None;
        self.latest = None;
        cx.notify();
        self._job = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { fetch_latest_release() })
                .await;
            let _ = this.update(cx, |this: &mut Self, cx| {
                this.checking = false;
                match result {
                    Ok(release) => this.latest = Some(release),
                    Err(err) => this.update_error = Some(err),
                }
                cx.notify();
            });
        }));
    }

    // ── ↑↓ 导航 ──

    fn move_page(&mut self, delta: i32, cx: &mut Context<Self>) {
        let len = ALL_PAGES.len() as i32;
        let idx = ALL_PAGES.iter().position(|p| *p == self.page).unwrap_or(0) as i32;
        self.page = ALL_PAGES[(((idx + delta) % len + len) % len) as usize];
        cx.notify();
    }
}

/// 这次输入事件算不算「提交」(失焦 / 回车)。
fn commits(event: &InputEvent) -> bool {
    matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. })
}

/// 终端字体输入框的 placeholder(`terminalCache.ts:50-51` 的
/// `DEFAULT_TERMINAL_FONT_FAMILY`)。
const DEFAULT_TERMINAL_FONT_PLACEHOLDER: &str =
    "'JetBrainsMono Nerd Font', 'CaskaydiaCove Nerd Font', 'JetBrains Mono', Consolas";

// ─── 渲染 ─────────────────────────────────────────────────────

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("settings-root")
            .size_full()
            .flex()
            .overflow_hidden()
            .track_focus(&self.focus)
            .key_context("SettingsPanel")
            // ↑/↓ 在扁平化分页序列里环形移动(原版挂在 tablist 上的 onKeyDown)。
            // 焦点跑进某个输入框时收不到这两个键 —— 组件库的单行输入框自己吃掉了,
            // 与原版「焦点在 tab 按钮上才响应」等效。
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                match event.keystroke.key.as_str() {
                    "up" => this.move_page(-1, cx),
                    "down" => this.move_page(1, cx),
                    _ => {}
                }
            }))
            .child(self.render_menu(cx))
            .child(
                div()
                    .id("settings-page")
                    .flex_1()
                    .h_full()
                    .overflow_y_scroll()
                    .px(px(20.0))
                    .py(px(16.0))
                    .child(self.render_page(cx)),
            )
    }
}

/// 页根节点:`space-y-6`。
fn page_root() -> gpui::Div {
    div().flex().flex_col().gap(px(24.0))
}

/// 分节:标题 + `space-y-2` 的内容。
fn section(title_key: &'static str) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(ui::settings_section_title(t("settings", title_key)))
}

/// 一行开关。`disabled` 时置灰**并且不挂点击**(gpui 没有 pointer-events)。
fn toggle_row(
    id: &'static str,
    title_key: &'static str,
    desc_key: &'static str,
    checked: bool,
    disabled: bool,
    on_toggle: impl Fn(&mut SettingsView, bool, &mut Window, &mut Context<SettingsView>) + 'static,
    cx: &mut Context<SettingsView>,
) -> gpui::Div {
    let control = ui::toggle(id, checked).when(!disabled, |el| {
        el.on_click(cx.listener(move |this, _, window, cx| {
            on_toggle(this, !checked, window, cx);
        }))
    });
    ui::setting_row(
        t("settings", title_key),
        Some(ui::desc_text(t("settings", desc_key)).into_any_element()),
        disabled,
        control,
    )
}

/// 一行数字输入(草稿态)。宽度 `w-24`、等宽右对齐,与原版一致。
fn number_row(
    title_key: &'static str,
    desc_key: &'static str,
    input: &Entity<InputState>,
    disabled: bool,
) -> gpui::Div {
    ui::setting_row(
        t("settings", title_key),
        Some(ui::desc_text(t("settings", desc_key)).into_any_element()),
        disabled,
        div().w(px(96.0)).child(Input::new(input)),
    )
}

impl SettingsView {
    fn render_menu(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let mut menu = div()
            .id("settings-menu")
            .w(px(172.0))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(ui::border_subtle())
            .py(px(12.0))
            .px(px(8.0))
            .flex()
            .flex_col();

        for (gi, (title_key, pages)) in MENU_GROUPS.iter().enumerate() {
            menu = menu.child(if title_key.is_empty() {
                // 空标题 = 一条分隔线(`mx-3 my-2 border-t`)
                div()
                    .mx(px(12.0))
                    .my(px(8.0))
                    .h(px(1.0))
                    .bg(ui::border_subtle())
            } else {
                div()
                    .px(px(12.0))
                    .pb(px(4.0))
                    .when(gi > 0, |el| el.pt(px(16.0)))
                    .text_size(ui::font_px(11.0))
                    .text_color(ui::text_muted())
                    .child(t("settings", title_key))
            });

            for page in *pages {
                let page = *page;
                let active = self.page == page;
                menu = menu.child(
                    div()
                        .id(SharedString::from(format!("settings-tab-{}", page.id())))
                        .w_full()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .px(px(12.0))
                        .py(px(8.0))
                        .rounded(px(4.0))
                        .cursor_pointer()
                        .text_size(ui::font_px(13.0))
                        .when(active, |el| {
                            el.bg(ui::accent_subtle()).text_color(ui::accent())
                        })
                        .when(!active, |el| {
                            el.text_color(ui::text_secondary()).hover(|el| {
                                el.bg(ui::border_subtle()).text_color(ui::text_primary())
                            })
                        })
                        // 左侧激活竖条:**未选中时留位不留色** —— 切页时标签文字
                        // 不会横向抖一下(原版 :2150 的注释)
                        .child(
                            div()
                                .w(px(2.0))
                                .h(px(16.0))
                                .flex_none()
                                .rounded(px(1.0))
                                .when(active, |el| el.bg(ui::accent())),
                        )
                        .child(div().truncate().child(t("settings", page.label_key())))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.page = page;
                            cx.notify();
                        })),
                );
            }
        }
        menu.into_any_element()
    }

    fn render_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        match self.page {
            SettingsPage::Terminal => self.render_terminal_page(cx),
            SettingsPage::Clipboard => self.render_clipboard_page(cx),
            SettingsPage::Appearance => self.render_appearance_page(cx),
            SettingsPage::Font => self.render_font_page(cx),
            SettingsPage::AiNotification => self.render_notification_page(cx),
            SettingsPage::AiHook => self.render_hook_page(cx),
            SettingsPage::System => self.render_system_page(cx),
            SettingsPage::Editor => self.render_editor_page(cx),
            SettingsPage::Shortcuts => self.render_shortcuts_page(cx),
            SettingsPage::About => self.render_about_page(cx),
        }
    }

    // ── terminal 页 ──

    fn render_terminal_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let list = self.store.read(cx).shell_list();
        let editing = self.shell_editing;

        let mut rows = div().flex().flex_col().gap(px(8.0));
        for (idx, shell) in list.shells.iter().enumerate() {
            if editing == Some(Some(idx)) {
                rows = rows.child(self.render_shell_form(cx));
                continue;
            }
            let is_default = shell.name == list.default_shell;
            let detail = match &shell.args {
                Some(args) if !args.is_empty() => format!("{} {}", shell.command, args.join(" ")),
                _ => shell.command.clone(),
            };
            rows = rows.child(
                ui::settings_card()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        radio_dot(format!("shell-default-{idx}"), is_default).on_click(cx.listener(
                            move |this, _, _window, cx| {
                                let name = this.store.read(cx).config().available_shells[idx]
                                    .name
                                    .clone();
                                this.store.update(cx, |store, cx| {
                                    let mut list = store.shell_list();
                                    list.set_default(&name);
                                    store.apply_shell_list(list, cx);
                                });
                            },
                        )),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(shell.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(detail),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(4.0))
                            .child(
                                ui::ghost_button(
                                    SharedString::from(format!("shell-edit-{idx}")),
                                    t("settings", "common.edit"),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let shell = this
                                        .store
                                        .read(cx)
                                        .config()
                                        .available_shells
                                        .get(idx)
                                        .cloned();
                                    this.shell_editing = Some(Some(idx));
                                    this.fill_shell_form(shell.as_ref(), window, cx);
                                })),
                            )
                            .child(
                                ui::danger_button(
                                    SharedString::from(format!("shell-del-{idx}")),
                                    t("settings", "common.delete"),
                                )
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.store.update(cx, |store, cx| {
                                        let mut list = store.shell_list();
                                        list.remove(idx);
                                        store.apply_shell_list(list, cx);
                                    });
                                    // 编辑中的行号会被这次删除搞错位,一并收掉表单
                                    this.shell_editing = None;
                                    cx.notify();
                                })),
                            ),
                    ),
            );
        }
        if editing == Some(None) {
            rows = rows.child(self.render_shell_form(cx));
        }

        page_root()
            .child(
                section("terminal.availableTerminals")
                    .child(rows)
                    .child(
                        dashed_button("shell-add", t("settings", "terminal.addTerminal")).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.shell_editing = Some(None);
                                this.fill_shell_form(None, window, cx);
                            }),
                        ),
                    )
                    .child(ui::hint(t("settings", "terminal.defaultHint"))),
            )
            .child(section("terminal.behavior").child(number_row(
                "terminal.scrollback",
                "terminal.scrollbackDesc",
                &self.num_scrollback,
                false,
            )))
            .into_any_element()
    }

    fn fill_shell_form(
        &mut self,
        shell: Option<&ShellConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, command, args) = match shell {
            Some(s) => (
                s.name.clone(),
                s.command.clone(),
                s.args.clone().unwrap_or_default().join(" "),
            ),
            None => (String::new(), String::new(), String::new()),
        };
        // 占位串在建输入框时只取过一次;这里重设一遍,免得面板开着的时候切了语言
        // (语言段控件就在同一个面板里),下次点「添加终端」还是旧语言。
        self.shell_name.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "terminal.newNamePlaceholder"), window, cx);
            s.set_value(name, window, cx);
        });
        self.shell_command.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "terminal.newCommandPlaceholder"), window, cx);
            s.set_value(command, window, cx);
        });
        self.shell_args.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "terminal.newArgsPlaceholder"), window, cx);
            s.set_value(args, window, cx);
        });
        self.shell_error = None;
        cx.notify();
    }

    fn render_shell_form(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let adding = self.shell_editing == Some(None);
        form_card(adding)
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    // 原版是 `flex-1` : `flex-[2]`;gpui 没有任意 grow 系数,
                    // 名称列给固定宽、命令列吃掉剩余 —— 同样的 1:2 观感
                    .child(div().w(px(150.0)).flex_none().child(Input::new(&self.shell_name)))
                    .child(div().flex_1().child(Input::new(&self.shell_command))),
            )
            .child(Input::new(&self.shell_args))
            .when_some(self.shell_error, |el, msg| {
                el.child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::color_error())
                        .child(msg),
                )
            })
            .child(
                div()
                    .flex()
                    .gap(px(6.0))
                    .child(
                        ui::primary_button(
                            "shell-save",
                            if adding {
                                t("settings", "common.add")
                            } else {
                                t("settings", "common.save")
                            },
                        )
                        .on_click(cx.listener(|this, _, _window, cx| this.save_shell(cx))),
                    )
                    .child(
                        ui::ghost_button("shell-cancel", t("settings", "common.cancel")).on_click(
                            cx.listener(|this, _, _window, cx| {
                                this.shell_editing = None;
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn save_shell(&mut self, cx: &mut Context<Self>) {
        let name = self.shell_name.read(cx).value().trim().to_string();
        let command = self.shell_command.read(cx).value().trim().to_string();
        if !valid_shell(&name, &command) {
            // 原版是「名字/命令为空时保存按钮直接不响应」,没有这句提示文案 ——
            // 借用 envVars 里语义最近的那条通用校验串。
            self.shell_error = Some(t("envVars", "hasErrors"));
            cx.notify();
            return;
        }
        let shell = ShellConfig {
            name,
            command,
            args: parse_args(&self.shell_args.read(cx).value()),
        };
        let editing = self.shell_editing;
        self.store.update(cx, |store, cx| {
            let mut list = store.shell_list();
            match editing {
                Some(Some(idx)) => list.update(idx, shell),
                _ => list.add(shell),
            }
            store.apply_shell_list(list, cx);
        });
        self.shell_editing = None;
        cx.notify();
    }

    // ── clipboard 页 ──

    fn render_clipboard_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let smart = config.smart_copy_paste;
        let long_paste = config.long_paste_to_file;

        page_root()
            .child(
                section("clipboard.copyPaste")
                    .child(toggle_row(
                        "clip-smart",
                        "clipboard.smartCopyPasteTitle",
                        "clipboard.smartCopyPasteDesc",
                        smart,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.smart_copy_paste = next, cx)
                            });
                        },
                        cx,
                    ))
                    .child(number_row(
                        "clipboard.autoCopyDwellTitle",
                        "clipboard.autoCopyDwellDesc",
                        &self.num_dwell,
                        false,
                    )),
            )
            .child(
                section("clipboard.longPaste")
                    .child(toggle_row(
                        "clip-long-paste",
                        "clipboard.longPasteTitle",
                        "clipboard.longPasteDesc",
                        long_paste,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.long_paste_to_file = next, cx)
                            });
                        },
                        cx,
                    ))
                    // 总开关关掉时下面两行**置灰**(与 system 页的托盘子项不同,
                    // 那边是整个不渲染)
                    .child(number_row(
                        "clipboard.lineThreshold",
                        "clipboard.lineThresholdDesc",
                        &self.num_line_threshold,
                        !long_paste,
                    ))
                    .child(number_row(
                        "clipboard.charThreshold",
                        "clipboard.charThresholdDesc",
                        &self.num_char_threshold,
                        !long_paste,
                    ))
                    .child(ui::hint(t("settings", "clipboard.longPasteFooter"))),
            )
            .child(
                section("clipboard.remotePaste")
                    // 这一段不是 SettingRow,是一张标题 + 说明 + 整宽输入框的卡片
                    .child(
                        ui::settings_card()
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(t("settings", "clipboard.remotePasteDir")),
                            )
                            .child(
                                div()
                                    .mb(px(8.0))
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(t("settings", "clipboard.remotePasteDirDesc")),
                            )
                            .child(Input::new(&self.txt_remote_paste_dir)),
                    )
                    .child(ui::hint(t("settings", "clipboard.remotePasteFooter"))),
            )
            .into_any_element()
    }

    // ── appearance 页 ──

    fn render_appearance_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let custom = config.custom_theme_id.clone();
        let theme = config.theme.clone();
        let skin = config.skin.clone();
        let follow = config.terminal_follow_theme;

        // 主题段:激活外置皮肤时三个按钮全不高亮
        let theme_value = choice_value(custom.as_deref(), &theme).to_string();
        let mut theme_group = choice_group();
        for (value, label_key) in [
            ("dark", "appearance.themeDark"),
            ("light", "appearance.themeLight"),
            ("auto", "appearance.themeAuto"),
        ] {
            let selected = theme_value == value;
            theme_group = theme_group.child(
                ui::choice_button(
                    SharedString::from(format!("theme-{value}")),
                    t("settings", label_key),
                    selected,
                    false,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    // 切主题 = 退出外置皮肤(`set_theme_mode` 内部自己清
                    // `custom_theme_id`,页面侧不必再清一遍)
                    this.store
                        .update(cx, |store, cx| store.set_theme_mode(value, window, cx));
                })),
            );
        }

        // 皮肤段:GPUI 侧没有内置皮肤色表,blueprint / fluent2 **置灰**
        let skin_value = choice_value(custom.as_deref(), &skin).to_string();
        let mut skin_group = choice_group();
        for (value, label, available) in [
            ("none", t("settings", "appearance.skinNone"), true),
            ("blueprint", t("settings", "appearance.skinBlueprint"), false),
            // 原版这一项是字面量,不走 i18n(`SettingsModal.tsx:849`)
            ("fluent2", "Fluent 2", false),
        ] {
            let selected = skin_value == value;
            skin_group = skin_group.child(
                ui::choice_button(
                    SharedString::from(format!("skin-{value}")),
                    label,
                    selected,
                    !available,
                )
                .when(available, |el| {
                    el.on_click(cx.listener(move |this, _, window, cx| {
                        this.store.update(cx, |store, cx| {
                            store.patch_config(|c| c.skin = value.to_string(), cx);
                            // 与 handleSkinChange 同序:清皮肤 → 重装主题
                            store.set_theme_pack(None, window, cx);
                        });
                    }))
                }),
            );
        }

        page_root()
            .child(section("appearance.language").child(ui::setting_row(
                t("settings", "appearance.languageLabel"),
                None,
                false,
                self.render_language_toggle(cx),
            )))
            .child(
                section("appearance.theme")
                    .child(theme_group)
                    .child(toggle_row(
                        "terminal-follow-theme",
                        "appearance.terminalFollowTheme",
                        "appearance.terminalFollowThemeDesc",
                        follow,
                        false,
                        |this, next, window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.set_terminal_follow_theme(next, window, cx)
                            });
                        },
                        cx,
                    )),
            )
            .child(
                section("appearance.skin")
                    .child(skin_group)
                    .child(ui::hint(t("settings", "appearance.skinDesc")))
                    .child(ui::hint(t("settings", "appearance.skinUnavailable"))),
            )
            .child(self.render_theme_packs(cx))
            .into_any_element()
    }

    /// 语言切换段控件。逐条对照 `src/components/LanguageToggle.tsx`:
    /// 两个选项、各写各自的**母语名**(中文 / English —— endonym 永不翻译)、
    /// 选中项 accent 底色白字,未选中透明底淡字。
    fn render_language_toggle(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current = self.store.read(cx).locale();
        let mut seg = div()
            .flex()
            .rounded(px(4.0))
            .overflow_hidden()
            .border_1()
            .border_color(ui::border_default());
        for option in Locale::ALL {
            let active = option == current;
            seg = seg.child(
                div()
                    .id(SharedString::from(format!("lang-{}", option.code())))
                    .px(px(12.0))
                    .py(px(3.0))
                    .text_size(ui::font_px(11.0))
                    .cursor_pointer()
                    .when(active, |el| {
                        el.bg(ui::accent()).text_color(ui::bg_base())
                    })
                    .when(!active, |el| {
                        el.text_color(ui::text_muted())
                            .hover(|el| el.text_color(ui::text_primary()))
                    })
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.store.update(cx, |store, cx| store.set_locale(option, cx));
                    }))
                    // 永远显示母语名,不随当前语言变
                    .child(option.native_name()),
            );
        }
        seg.into_any_element()
    }

    /// 外置皮肤段(原版 `CustomThemePacksSection`)。
    fn render_theme_packs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.store.read(cx).config().custom_theme_id.clone();

        // 标题行 + 五个小按钮。`flex_wrap` 允许换行 —— 680px 弹窗里英文文案会贴边
        let actions = div()
            .flex()
            .flex_wrap()
            .gap(px(8.0))
            .child(
                ui::ghost_button("theme-add", t("settings", "themes.addPack")).on_click(
                    cx.listener(|this, _, _window, cx| this.import_theme_dir(cx)),
                ),
            )
            .child(
                ui::ghost_button("theme-zip", t("settings", "themes.importZip")).on_click(
                    cx.listener(|this, _, _window, cx| this.import_theme_zip(cx)),
                ),
            )
            .child(
                ui::ghost_button("theme-example", t("settings", "themes.createExample")).on_click(
                    cx.listener(|this, _, _window, cx| {
                        this.run_theme_job(
                            |packs| packs.create_example().map_err(|e| format!("{e:#}")),
                            Some(|id| tr!("settings", "themes.exampleCreated", id = id)),
                            cx,
                        );
                    }),
                ),
            )
            .child(
                ui::ghost_button("theme-open-dir", t("settings", "themes.openDir")).on_click(
                    cx.listener(|this, _, _window, cx| {
                        let root = crate::theme::theme_packs().root().to_path_buf();
                        let _ = std::fs::create_dir_all(&root);
                        if let Err(err) = crate::fs_ops::reveal_in_file_manager(&root) {
                            this.theme_error = Some(err.to_string());
                            cx.notify();
                        }
                    }),
                ),
            )
            .child(
                ui::ghost_button("theme-refresh", t("settings", "themes.refresh")).on_click(
                    cx.listener(|this, _, _window, cx| this.refresh_theme_packs(cx)),
                ),
            );

        let list: AnyElement = if self.theme_cards.is_empty() {
            ui::settings_card()
                .py(px(16.0))
                .child(
                    div()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(t("settings", "themes.empty")),
                )
                .into_any_element()
        } else {
            // 原版 `grid grid-cols-2 gap-2`;gpui 没有 grid,用可换行的 flex 铺
            let mut grid = div().flex().flex_wrap().gap(px(8.0));
            for (idx, card) in self.theme_cards.iter().enumerate() {
                grid = grid.child(self.render_theme_card(idx, card, active.as_deref(), cx));
            }
            grid.into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap(px(8.0))
                    .child(ui::settings_section_title(t(
                        "settings",
                        "themes.customSection",
                    )))
                    .child(actions),
            )
            .child(list)
            .when_some(self.theme_error.clone(), |el, err| {
                el.child(banner(err, ui::color_error()))
            })
            // notice 与 error 互斥展示(`notice && !error`)
            .when(self.theme_error.is_none(), |el| {
                el.when_some(self.theme_notice.clone(), |el, msg| {
                    el.child(banner(msg, ui::color_success()))
                })
            })
            .into_any_element()
    }

    /// 一张皮肤卡片:缩小版的界面预览 + 名称 + hover 才出现的删除。
    fn render_theme_card(
        &self,
        idx: usize,
        card: &ThemeCard,
        active_id: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let active = active_id == Some(card.theme_id.as_str());
        let theme_id = card.theme_id.clone();
        let name = card.name.clone();

        let preview = div()
            .relative()
            .w_full()
            .h(px(96.0))
            .rounded(px(4.0))
            .overflow_hidden()
            .border_1()
            .border_color(ui::border_subtle())
            .bg(card.background)
            .when_some(card.image.clone(), |el, path| {
                el.child(img(path).absolute().inset_0().size_full())
            })
            // 压暗层,与真实氛围层同款(35%)
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .bg(ui::with_alpha(card.background, 0.35)),
            )
            // 迷你侧栏(72% 半透明面板)
            .child(
                div()
                    .absolute()
                    .left(px(6.0))
                    .top(px(6.0))
                    .bottom(px(6.0))
                    .w(px(48.0))
                    .rounded(px(3.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .bg(ui::with_alpha(card.panel, 0.72))
                    .child(mini_bar(32.0, card.accent, 1.0))
                    .child(mini_bar(24.0, card.text, 0.6))
                    .child(mini_bar(28.0, card.text, 0.4)),
            )
            // 迷你终端区(60% 着色 + 提示符)
            .child(
                div()
                    .absolute()
                    .left(px(62.0))
                    .right(px(6.0))
                    .top(px(6.0))
                    .bottom(px(6.0))
                    .rounded(px(3.0))
                    .px(px(6.0))
                    .py(px(4.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .bg(ui::with_alpha(card.background, 0.6))
                    .child(
                        div()
                            .flex()
                            .gap(px(3.0))
                            .text_size(px(10.0))
                            .child(div().text_color(card.accent).child("\u{276f}"))
                            .child(div().text_color(card.text).child("Aa 字")),
                    )
                    .child(mini_bar(40.0, card.text, 0.5)),
            );

        div()
            .id(SharedString::from(format!("theme-card-{idx}")))
            .group(SharedString::from(format!("theme-card-group-{idx}")))
            .w(px(300.0))
            .flex()
            .flex_col()
            .gap(px(8.0))
            .p(px(12.0))
            .rounded(px(6.0))
            .border_1()
            .cursor_pointer()
            .when(active, |el| {
                el.border_color(ui::accent()).bg(ui::accent_subtle())
            })
            .when(!active, |el| {
                el.border_color(ui::border_default()).bg(ui::bg_base())
            })
            .child(preview)
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap(px(8.0))
                    .child(
                        div()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(if active {
                                        ui::accent()
                                    } else {
                                        ui::text_primary()
                                    })
                                    .child(name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(theme_id.clone()),
                            ),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("theme-del-{idx}")))
                            .px(px(4.0))
                            .flex_none()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .cursor_pointer()
                            .hover(|el| el.text_color(ui::color_error()))
                            .child("\u{2715}")
                            .on_click(cx.listener({
                                let theme_id = theme_id.clone();
                                let name = name.clone();
                                move |this, _, window, cx| {
                                    // 卡片本身也有 on_click(选中),不拦住会连带选中
                                    cx.stop_propagation();
                                    this.confirm_delete_pack(
                                        theme_id.clone(),
                                        name.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, _, window, cx| {
                // **先装上主题、成功了才写配置**:装不上 `set_theme_pack` 返回 false
                // 且不落盘(内存里已回落内置)
                let ok = this.store.update(cx, |store, cx| {
                    store.set_theme_pack(Some(theme_id.clone()), window, cx)
                });
                if !ok {
                    this.theme_error = Some(tr!(
                        "settings",
                        "themes.applyFailed",
                        detail = theme_id.clone()
                    ));
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn confirm_delete_pack(
        &mut self,
        theme_id: String,
        name: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity();
        let store = self.store.clone();
        Confirm::new(
            t("settings", "themes.customSection"),
            tr!("settings", "themes.deleteConfirm", name = name),
        )
        .ok_text(t("settings", "common.delete"))
        .open(
            move |window, cx| {
                // **先退出该主题再删目录**:反过来的话 notify 的目录句柄还开着,
                // 被删目录在 Windows 上处于 delete-pending,紧接着重导入同名主题
                // 会撞 ERROR_ACCESS_DENIED(原版 :1886-1888 记的坑)
                let was_active =
                    store.read(cx).config().custom_theme_id.as_deref() == Some(theme_id.as_str());
                if was_active {
                    store.update(cx, |store, cx| {
                        store.set_theme_pack(None, window, cx);
                    });
                }
                let packs = crate::theme::theme_packs();
                let id = theme_id.clone();
                view.update(cx, |this: &mut SettingsView, cx| {
                    this._job = Some(cx.spawn(async move |this, cx| {
                        let result = cx
                            .background_executor()
                            .spawn(async move { packs.delete(&id).map_err(|e| format!("{e:#}")) })
                            .await;
                        let _ = this.update(cx, |this: &mut SettingsView, cx| {
                            this.refresh_theme_packs(cx);
                            if let Err(err) = result {
                                this.theme_error = Some(err);
                            }
                            cx.notify();
                        });
                    }));
                });
            },
            window,
            cx,
        );
    }

    fn import_theme_dir(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(t("settings", "themes.importDialogTitle").into()),
        });
        self._job = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(dir) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.run_theme_job(
                    move |packs| packs.import_dir(&dir).map_err(|e| format!("{e:#}")),
                    None,
                    cx,
                );
            });
        }));
    }

    fn import_theme_zip(&mut self, cx: &mut Context<Self>) {
        // gpui 的选择框**没有扩展名过滤**(`PathPromptOptions` 只有四个字段),
        // 选错文件由 `import_zip` 自己报错
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("settings", "themes.importZipDialogTitle").into()),
        });
        self._job = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(zip) = paths.into_iter().next() else {
                return;
            };
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.run_theme_job(
                    move |packs| packs.import_zip(&zip).map_err(|e| format!("{e:#}")),
                    None,
                    cx,
                );
            });
        }));
    }

    // ── font 页 ──

    fn render_font_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let ui_size = config.ui_font_size as i32;
        let term_size = config.terminal_font_size as i32;
        let ligatures = config.terminal_ligatures;

        let store_ui = self.store.clone();
        let store_term = self.store.clone();

        page_root()
            .child(
                section("font.fontSize")
                    .child(ui::font_size_slider(
                        "ui-font-size",
                        t("settings", "font.uiFontSize"),
                        ui_size,
                        10,
                        20,
                        move |value, _window, cx| {
                            store_ui.update(cx, |store, cx| {
                                store.set_ui_font_size(value as f64, cx);
                            });
                        },
                    ))
                    .child(ui::font_size_slider(
                        "terminal-font-size",
                        t("settings", "font.terminalFontSize"),
                        term_size,
                        10,
                        24,
                        move |value, _window, cx| {
                            store_term.update(cx, |store, cx| {
                                store.set_terminal_font_size(value as f64, cx);
                            });
                        },
                    ))
                    .child(ui::hint(t("settings", "font.fontSizeFooter"))),
            )
            .child(
                section("font.font")
                    .child(font_family_input(
                        t("settings", "font.uiFont"),
                        &self.txt_ui_font,
                    ))
                    .child(font_family_input(
                        t("settings", "font.terminalFont"),
                        &self.txt_terminal_font,
                    ))
                    .child(ui::hint(format!(
                        "{}'JetBrainsMono Nerd Font', monospace{}",
                        t("settings", "font.fontFamilyFooterPrefix"),
                        t("settings", "font.fontFamilyFooterSuffix"),
                    ))),
            )
            .child(
                section("font.ligatures")
                    // 底层没有连字:整行置灰(不挂 on_click),下面一句说明为什么
                    .child(ui::setting_row(
                        t("settings", "font.ligaturesTitle"),
                        Some(
                            ui::desc_text(format!(
                                "{}== => != ->{}",
                                t("settings", "font.ligaturesDescPrefix"),
                                t("settings", "font.ligaturesDescSuffix"),
                            ))
                            .into_any_element(),
                        ),
                        true,
                        ui::toggle("font-ligatures", ligatures),
                    ))
                    .child(ui::hint(t("settings", "font.ligaturesUnavailable"))),
            )
            .into_any_element()
    }

    // ── ai-notification 页 ──

    fn render_notification_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let popup = config.ai_completion_popup;
        let flash = config.ai_completion_taskbar_flash;
        let sound = config.ai_completion_sound;
        let sound_path = config.ai_completion_sound_path.clone();
        let attention = config.ai_attention_notify;

        let path_label = sound_path
            .clone()
            .unwrap_or_else(|| t("settings", "aiNotification.defaultSound").to_string());

        let mut buttons = div()
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(
                ui::ghost_button("sound-preview", t("settings", "aiNotification.preview"))
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        let path = this.store.read(cx).config().ai_completion_sound_path.clone();
                        crate::notify::play_sound(path.as_deref());
                    })),
            )
            .child(
                ui::ghost_button("sound-choose", t("settings", "aiNotification.chooseFile"))
                    .on_click(cx.listener(|this, _, _window, cx| this.choose_sound_file(cx))),
            );
        // 「清除」仅当已有自定义路径时才渲染(原版 :1319)
        if sound_path.is_some() {
            buttons = buttons.child(
                ui::danger_button("sound-clear", t("settings", "aiNotification.clear")).on_click(
                    cx.listener(|this, _, _window, cx| {
                        this.sound_warning = false;
                        this.store.update(cx, |store, cx| {
                            store.patch_config(|c| c.ai_completion_sound_path = None, cx)
                        });
                    }),
                ),
            );
        }

        page_root()
            .child(
                section("aiNotification.method")
                    .child(toggle_row(
                        "notify-popup",
                        "aiNotification.popup",
                        "aiNotification.popupDesc",
                        popup,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.ai_completion_popup = next, cx)
                            });
                        },
                        cx,
                    ))
                    .child(toggle_row(
                        "notify-flash",
                        "aiNotification.taskbarFlash",
                        "aiNotification.taskbarFlashDesc",
                        flash,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.ai_completion_taskbar_flash = next, cx)
                            });
                        },
                        cx,
                    ))
                    .child(toggle_row(
                        "notify-sound",
                        "aiNotification.sound",
                        "aiNotification.soundDesc",
                        sound,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.ai_completion_sound = next, cx)
                            });
                        },
                        cx,
                    ))
                    // 提示音总开关关掉时整行置灰
                    .child(ui::setting_row(
                        t("settings", "aiNotification.customSound"),
                        Some(ui::desc_text(path_label).truncate().into_any_element()),
                        !sound,
                        buttons,
                    ))
                    // GPUI 侧的提示音只认 .wav(`notify.rs:234-267`),其余静默回落
                    // 系统提示音 —— 选到别的格式时把这条说出来
                    .when(self.sound_warning, |el| {
                        el.child(banner(
                            t("settings", "aiNotification.wavOnly").to_string(),
                            ui::color_warning(),
                        ))
                    })
                    .child(ui::hint(t("settings", "aiNotification.footer"))),
            )
            .child(
                section("aiNotification.trigger")
                    .child(toggle_row(
                        "notify-attention",
                        "aiNotification.attention",
                        "aiNotification.attentionDesc",
                        attention,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.ai_attention_notify = next, cx)
                            });
                        },
                        cx,
                    ))
                    .child(ui::hint(t("settings", "aiNotification.attentionFooter"))),
            )
            .into_any_element()
    }

    fn choose_sound_file(&mut self, cx: &mut Context<Self>) {
        // gpui 的选择框没有扩展名过滤(`PathPromptOptions` 只有四个字段),
        // 原版那 6 种格式的 filter 做不到 —— 选完自己校验
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("settings", "aiNotification.soundDialogTitle").into()),
        });
        self._job = Some(cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let is_wav = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("wav"))
                .unwrap_or(false);
            let text = path.to_string_lossy().to_string();
            let _ = this.update(cx, |this: &mut SettingsView, cx| {
                this.sound_warning = !is_wav;
                this.store.update(cx, |store, cx| {
                    store.patch_config(|c| c.ai_completion_sound_path = Some(text), cx)
                });
                cx.notify();
            });
        }));
    }

    // ── ai-hook 页 ──

    fn render_hook_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let enabled = self.store.read(cx).config().hook_enabled;
        let agents = self.agents();
        let busy = self.hook_busy;

        // 注入目标列表
        let mut targets = ui::settings_card().p_0().flex().flex_col().child(
            div()
                .px(px(12.0))
                .pt(px(10.0))
                .pb(px(4.0))
                .text_size(ui::font_px(11.0))
                .text_color(ui::text_muted())
                .child(t("settings", "aiHook.targetsLabel")),
        );
        for reg in &self.registrations {
            let checked = agents.contains(&reg.agent);
            let (badge, color) = if reg.registered == 0 {
                (
                    t("settings", "aiHook.stateAbsent").to_string(),
                    ui::text_muted(),
                )
            } else if reg.registered < reg.total {
                (
                    tr!(
                        "settings",
                        "aiHook.stateStale",
                        n = reg.registered,
                        total = reg.total
                    ),
                    ui::color_warning(),
                )
            } else {
                (
                    tr!("settings", "aiHook.stateReady", n = reg.total),
                    ui::color_success(),
                )
            };
            let agent = reg.agent.clone();
            targets = targets.child(
                div()
                    .id(SharedString::from(format!("hook-target-{}", reg.agent)))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .cursor_pointer()
                    .hover(|el| el.bg(ui::border_subtle()))
                    .child(ui::checkbox(
                        SharedString::from(format!("hook-check-{}", reg.agent)),
                        checked,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(reg.label.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(reg.file.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(ui::font_px(11.0))
                            .text_color(color)
                            .child(badge),
                    )
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        let mut list = this.agents();
                        match list.iter().position(|a| *a == agent) {
                            Some(idx) => {
                                list.remove(idx);
                            }
                            None => list.push(agent.clone()),
                        }
                        this.selected_agents = Some(list);
                        cx.notify();
                    })),
            );
        }

        // 开关关闭时整块置灰(错误条不受影响,见下)
        let body = div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .when(!enabled, |el| el.opacity(0.5))
            .child(
                ui::settings_card()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .mb(px(4.0))
                            .child(
                                div()
                                    .w(px(8.0))
                                    .h(px(8.0))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(if self.hook_running {
                                        ui::color_success()
                                    } else {
                                        ui::border_strong()
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(format!(
                                        "{} {}",
                                        t("settings", "aiHook.serverLabel"),
                                        if self.hook_running {
                                            tr!("settings", "aiHook.serverRunning", port = self.hook_port)
                                        } else {
                                            t("settings", "aiHook.serverStopped").to_string()
                                        }
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_muted())
                            .child(t("settings", "aiHook.serverDesc")),
                    ),
            )
            .child(targets)
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div().flex_1().child(
                            ui::primary_button(
                                "hook-register",
                                if busy {
                                    t("settings", "aiHook.registering")
                                } else {
                                    t("settings", "aiHook.register")
                                },
                            )
                            .py(px(8.0))
                            .when(busy || agents.is_empty(), |el| el.opacity(0.5))
                            .when(!busy && !agents.is_empty(), |el| {
                                el.on_click(cx.listener(|this, _, _window, cx| {
                                    this.run_hook_action(true, cx)
                                }))
                            }),
                        ),
                    )
                    .child(
                        div().flex_1().child(
                            ui::ghost_button(
                                "hook-unregister",
                                if busy {
                                    t("settings", "aiHook.unregistering")
                                } else {
                                    t("settings", "aiHook.unregister")
                                },
                            )
                            .py(px(8.0))
                            .when(busy || agents.is_empty(), |el| el.opacity(0.5))
                            .when(!busy && !agents.is_empty(), |el| {
                                el.on_click(cx.listener(|this, _, _window, cx| {
                                    this.run_hook_action(false, cx)
                                }))
                            }),
                        ),
                    ),
            )
            .when(agents.is_empty(), |el| {
                el.child(
                    div()
                        .flex()
                        .justify_center()
                        .text_size(ui::font_px(11.0))
                        .text_color(ui::text_muted())
                        .child(t("settings", "aiHook.noTargetSelected")),
                )
            })
            .child(
                div()
                    .id("hook-snippet-toggle")
                    .w_full()
                    .flex()
                    .justify_center()
                    .py(px(8.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_muted())
                    .hover(|el| el.text_color(ui::accent()))
                    .child(if self.show_snippet {
                        t("settings", "aiHook.collapseSnippet")
                    } else {
                        t("settings", "aiHook.showSnippet")
                    })
                    .on_click(cx.listener(|this, _, _window, cx| this.toggle_snippet(cx))),
            )
            .when(self.show_snippet, |el| {
                el.children(self.render_snippet(cx))
            })
            .child(ui::hint(t("settings", "aiHook.footer")));

        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(ui::settings_section_title(t("settings", "aiHook.title")))
            .child(toggle_row(
                "hook-enable",
                "aiHook.enableHook",
                "aiHook.enableHookDesc",
                enabled,
                false,
                |this, next, _window, cx| this.toggle_hook_server(next, cx),
                cx,
            ))
            // 结果/错误条**始终可见**,不受下面那块的置灰影响
            .when(!self.hook_result.is_empty(), |el| {
                el.child(
                    ui::settings_card().child(
                        div()
                            .text_size(ui::font_px(11.0))
                            .text_color(ui::text_secondary())
                            .children(
                                self.hook_result
                                    .split('\n')
                                    .map(|line| div().child(line.to_string()))
                                    .collect::<Vec<_>>(),
                            ),
                    ),
                )
            })
            .child(body)
            .into_any_element()
    }

    /// 配置片段面板(三个 tab,标签是字面量不走 i18n)。
    fn render_snippet(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let data = self.snippet.as_ref()?;
        let mut tabs = div()
            .flex()
            .border_b_1()
            .border_color(ui::border_subtle());
        for (key, label) in [
            ("claude", "Claude Code"),
            ("codex", "Codex"),
            ("grok", "Grok"),
        ] {
            let active = self.snippet_tab == key;
            tabs = tabs.child(
                div()
                    .id(SharedString::from(format!("snippet-tab-{key}")))
                    .flex_1()
                    .flex()
                    .justify_center()
                    .py(px(6.0))
                    .cursor_pointer()
                    .text_size(ui::font_px(11.0))
                    .when(active, |el| {
                        el.text_color(ui::accent()).border_b_2().border_color(ui::accent())
                    })
                    .when(!active, |el| el.text_color(ui::text_muted()))
                    .child(label)
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        this.snippet_tab = key;
                        cx.notify();
                    })),
            );
        }

        let mut content = div()
            .id("snippet-body")
            .px(px(12.0))
            .py(px(8.0))
            .max_h(px(256.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .text_size(ui::font_px(10.0))
            .text_color(ui::text_muted());
        let section_of = |value: &serde_json::Value, name: &str| value.get(name).cloned();
        if self.snippet_tab == "claude" {
            if let Some(claude) = section_of(data, "claude") {
                content = content
                    .child(snippet_file_name(
                        claude.get("file").and_then(|v| v.as_str()).unwrap_or(""),
                        None,
                    ))
                    .children(snippet_lines(
                        claude.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                    ));
            }
        } else if let Some(files) = section_of(data, self.snippet_tab)
            .and_then(|v| v.get("files").cloned())
            .and_then(|v| v.as_array().cloned())
        {
            for (i, file) in files.iter().enumerate() {
                content = content
                    .child(
                        snippet_file_name(
                            file.get("file").and_then(|v| v.as_str()).unwrap_or(""),
                            file.get("note").and_then(|v| v.as_str()),
                        )
                        .when(i > 0, |el| {
                            el.mt(px(12.0))
                                .pt(px(12.0))
                                .border_t_1()
                                .border_color(ui::border_subtle())
                        }),
                    )
                    .children(snippet_lines(
                        file.get("content").and_then(|v| v.as_str()).unwrap_or(""),
                    ));
            }
        }

        Some(
            div()
                .rounded(px(4.0))
                .border_1()
                .border_color(ui::border_default())
                .bg(ui::bg_base())
                .overflow_hidden()
                .child(tabs)
                .child(content)
                .into_any_element(),
        )
    }

    // ── system 页 ──

    fn render_system_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let tray = config.tray_status_enabled.unwrap_or(true);
        let click_focus = config.tray_click_focus.unwrap_or(true);
        let auto_resume = config.ai_auto_resume.unwrap_or(true);

        page_root()
            .child(
                section("system.trayGroup")
                    .child(toggle_row(
                        "tray-enabled",
                        "system.trayStatusTitle",
                        "system.trayStatusDesc",
                        tray,
                        false,
                        |this, next, _window, cx| {
                            this.store.update(cx, |store, cx| {
                                store.patch_config(|c| c.tray_status_enabled = Some(next), cx)
                            });
                        },
                        cx,
                    ))
                    // ⚠️ 总开关关掉时这两行**整个不渲染**(不是置灰)——
                    // 与 clipboard 页的处理不一样,原版就是这么写的
                    .when(tray_children_visible(tray), |el| {
                        el.child(toggle_row(
                            "tray-click-focus",
                            "system.trayClickFocusTitle",
                            "system.trayClickFocusDesc",
                            click_focus,
                            false,
                            |this, next, _window, cx| {
                                this.store.update(cx, |store, cx| {
                                    store.patch_config(|c| c.tray_click_focus = Some(next), cx)
                                });
                            },
                            cx,
                        ))
                        .child(number_row(
                            "system.trayMaxTitle",
                            "system.trayMaxDesc",
                            &self.num_tray_max,
                            false,
                        ))
                    }),
            )
            .child(section("system.startupGroup").child(toggle_row(
                "ai-auto-resume",
                "system.aiAutoResumeTitle",
                "system.aiAutoResumeDesc",
                auto_resume,
                false,
                |this, next, _window, cx| {
                    this.store.update(cx, |store, cx| {
                        store.patch_config(|c| c.ai_auto_resume = Some(next), cx)
                    });
                },
                cx,
            )))
            .into_any_element()
    }

    // ── editor 页 ──

    fn render_editor_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let config = self.store.read(cx).config();
        let editors = config.editors.clone();
        let default_editor = config.default_editor.clone().unwrap_or_default();
        let editing = self.editor_editing;

        let mut rows = div().flex().flex_col().gap(px(8.0));
        for (idx, editor) in editors.iter().enumerate() {
            if editing == Some(Some(idx)) {
                rows = rows.child(self.render_editor_form(cx));
                continue;
            }
            let is_default = editor.name == default_editor;
            rows = rows.child(
                ui::settings_card()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        radio_dot(format!("editor-default-{idx}"), is_default).on_click(
                            cx.listener(move |this, _, _window, cx| {
                                let name = this.store.read(cx).config().editors[idx].name.clone();
                                this.store.update(cx, |store, cx| {
                                    store.patch_config(|c| c.default_editor = Some(name), cx)
                                });
                            }),
                        ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(13.0))
                                    .text_color(ui::text_primary())
                                    .child(editor.name.clone()),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(ui::font_px(11.0))
                                    .text_color(ui::text_muted())
                                    .child(editor.command.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .gap(px(4.0))
                            .child(
                                ui::ghost_button(
                                    SharedString::from(format!("editor-edit-{idx}")),
                                    t("settings", "common.edit"),
                                )
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let editor =
                                        this.store.read(cx).config().editors.get(idx).cloned();
                                    this.editor_editing = Some(Some(idx));
                                    this.fill_editor_form(editor.as_ref(), window, cx);
                                })),
                            )
                            .child(
                                ui::danger_button(
                                    SharedString::from(format!("editor-del-{idx}")),
                                    t("settings", "common.delete"),
                                )
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    this.delete_editor(idx, cx);
                                })),
                            ),
                    ),
            );
        }
        if editing == Some(None) {
            rows = rows.child(self.render_editor_form(cx));
        }

        page_root()
            .child(
                section("editor.externalEditor")
                    .child(rows)
                    .child(
                        dashed_button("editor-add", t("settings", "editor.addEditor")).on_click(
                            cx.listener(|this, _, window, cx| {
                                this.editor_editing = Some(None);
                                this.fill_editor_form(None, window, cx);
                            }),
                        ),
                    )
                    .child(ui::hint(t("settings", "editor.editorDefaultHint"))),
            )
            .into_any_element()
    }

    fn fill_editor_form(
        &mut self,
        editor: Option<&EditorConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, command) = match editor {
            Some(e) => (e.name.clone(), e.command.clone()),
            None => (String::new(), String::new()),
        };
        self.editor_name.update(cx, |s, cx| {
            s.set_placeholder(t("settings", "editor.newEditorNamePlaceholder"), window, cx);
            s.set_value(name, window, cx);
        });
        self.editor_command.update(cx, |s, cx| {
            s.set_placeholder(
                t("settings", "editor.newEditorCommandPlaceholder"),
                window,
                cx,
            );
            s.set_value(command, window, cx);
        });
        cx.notify();
    }

    fn render_editor_form(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let adding = self.editor_editing == Some(None);
        form_card(adding)
            .child(Input::new(&self.editor_name))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().child(Input::new(&self.editor_command)))
                    // 「...」浏览按钮(shell 列表没有这一颗)
                    .child(
                        ui::ghost_button("editor-browse", "...").on_click(cx.listener(
                            |this, _, window, cx| this.browse_editor_path(window, cx),
                        )),
                    )
                    .child(
                        ui::primary_button(
                            "editor-save",
                            if adding {
                                t("settings", "common.add")
                            } else {
                                t("settings", "common.save")
                            },
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.save_editor(window, cx))),
                    )
                    .child(
                        ui::ghost_button("editor-cancel", t("settings", "common.cancel")).on_click(
                            cx.listener(|this, _, _window, cx| {
                                this.editor_editing = None;
                                cx.notify();
                            }),
                        ),
                    ),
            )
            .into_any_element()
    }

    fn browse_editor_path(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Windows 上原版带 `.exe` 过滤;gpui 的选择框没有过滤能力(见 §7 坑 2)
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(t("settings", "editor.browseDialogTitle").into()),
        });
        // `spawn_in` 而不是 `spawn`:回填输入框的 `set_value` 要 `&mut Window`,
        // 只有 `AsyncWindowContext` 给得出来
        self._job = Some(cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let text = path.to_string_lossy().to_string();
            let _ = this.update_in(cx, |this: &mut SettingsView, window, cx| {
                this.editor_command
                    .update(cx, |state, cx| state.set_value(text, window, cx));
            });
        }));
    }

    fn save_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.editor_name.read(cx).value().trim().to_string();
        let command = self.editor_command.read(cx).value().trim().to_string();
        if name.is_empty() || command.is_empty() {
            return;
        }
        let editing = self.editor_editing;
        let editors = self.store.read(cx).config().editors.clone();
        // **重名校验**:shell 列表没有这一条,编辑器列表有(原版 :1432/:1462)
        let clash = editors.iter().enumerate().any(|(i, e)| {
            e.name == name && editing.flatten() != Some(i)
        });
        if clash {
            show_alert(
                t("settings", "menu.editor"),
                tr!("settings", "editor.editorExistsAlert", name = name),
                window,
                cx,
            );
            return;
        }

        let mut editors = editors;
        let mut default_editor = self.store.read(cx).config().default_editor.clone();
        match editing {
            Some(Some(idx)) if idx < editors.len() => {
                let was_default = default_editor.as_deref() == Some(editors[idx].name.as_str());
                editors[idx] = EditorConfig {
                    name: name.clone(),
                    command,
                };
                if was_default {
                    default_editor = Some(name);
                }
            }
            _ => {
                editors.push(EditorConfig {
                    name: name.clone(),
                    command,
                });
                if default_editor.is_none() {
                    default_editor = Some(name);
                }
            }
        }
        self.store.update(cx, |store, cx| {
            store.patch_config(
                move |c| {
                    c.editors = editors;
                    c.default_editor = default_editor;
                },
                cx,
            )
        });
        self.editor_editing = None;
        cx.notify();
    }

    fn delete_editor(&mut self, idx: usize, cx: &mut Context<Self>) {
        let mut editors = self.store.read(cx).config().editors.clone();
        if idx >= editors.len() {
            return;
        }
        editors.remove(idx);
        let current = self.store.read(cx).config().default_editor.clone();
        // 删掉的正是默认项时落到剩下的第一个;**空列表写 `None` 而不是空串**
        let default_editor = match current {
            Some(name) if editors.iter().any(|e| e.name == name) => Some(name),
            _ => editors.first().map(|e| e.name.clone()),
        };
        self.store.update(cx, |store, cx| {
            store.patch_config(
                move |c| {
                    c.editors = editors;
                    c.default_editor = default_editor;
                },
                cx,
            )
        });
        self.editor_editing = None;
        cx.notify();
    }

    // ── shortcuts 页 ──

    fn render_shortcuts_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let smart = self.store.read(cx).config().smart_copy_paste;
        let mut root = page_root();

        for (group_key, items) in hotkeys::groups() {
            let mut rows = div().flex().flex_col().gap(px(4.0));
            for def in items {
                rows = rows.child(shortcut_row(
                    t("settings", def.desc_key),
                    hotkeys::combo_label(&def.combo),
                ));
            }
            // 智能 Ctrl+C/V 开启时才存在,附在「复制粘贴」组末尾
            if smart && group_key == "shortcuts.clipboard" {
                let modifier = hotkeys::combo_label(&hotkeys::Combo {
                    modifier: true,
                    shift: false,
                    alt: false,
                    key: "C",
                });
                let paste = hotkeys::combo_label(&hotkeys::Combo {
                    modifier: true,
                    shift: false,
                    alt: false,
                    key: "V",
                });
                rows = rows
                    .child(shortcut_row(t("settings", "shortcuts.copyDesc"), modifier))
                    .child(shortcut_row(
                        t("settings", "shortcuts.pasteToTerminal"),
                        paste,
                    ));
            }
            root = root.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(ui::settings_section_title(t("settings", group_key)))
                    .child(rows),
            );
        }

        root.child(ui::hint(t("settings", "shortcuts.footer")))
            .into_any_element()
    }

    // ── about 页 ──

    fn render_about_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let current = env!("CARGO_PKG_VERSION");
        let latest = self.latest.clone();
        let has_update = latest
            .as_ref()
            .is_some_and(|r| compare_versions(&r.version, current).is_gt());

        page_root()
            .child(ui::settings_section_title(t("settings", "about.versionInfo")))
            .child(
                ui::settings_card()
                    .px(px(16.0))
                    .py(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .child(
                        div()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::text_secondary())
                            .child(t("settings", "about.currentVersion")),
                    )
                    .child(
                        div()
                            .text_size(ui::font_px(13.0))
                            .text_color(ui::accent())
                            .child(format!("v{current}")),
                    ),
            )
            .child(
                div()
                    .id("about-check")
                    .w_full()
                    .flex()
                    .justify_center()
                    .py(px(10.0))
                    .rounded(px(6.0))
                    .border_1()
                    .border_color(ui::border_default())
                    .text_size(ui::font_px(13.0))
                    .text_color(ui::text_secondary())
                    .when(self.checking, |el| el.opacity(0.5))
                    .when(!self.checking, |el| {
                        el.cursor_pointer()
                            .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
                            .on_click(cx.listener(|this, _, _window, cx| this.check_update(cx)))
                    })
                    .child(if self.checking {
                        t("settings", "about.checking")
                    } else {
                        t("settings", "about.checkUpdate")
                    }),
            )
            .when_some(self.update_error.clone(), |el, err| {
                el.child(banner(err, ui::color_error()))
            })
            .when_some(latest, |el, release| {
                el.child(
                    ui::settings_card()
                        .px(px(16.0))
                        .py(px(12.0))
                        .when(has_update, |el| el.border_color(ui::accent()))
                        .child(if has_update {
                            div()
                                .flex()
                                .flex_col()
                                .gap(px(12.0))
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.0))
                                        .child(
                                            div()
                                                .text_size(ui::font_px(13.0))
                                                .text_color(ui::text_primary())
                                                .child(t("settings", "about.newVersionFound")),
                                        )
                                        .child(
                                            div()
                                                .text_size(ui::font_px(13.0))
                                                .text_color(ui::accent())
                                                .child(release.version.clone()),
                                        ),
                                )
                                .child(
                                    div()
                                        .text_size(ui::font_px(11.0))
                                        .text_color(ui::text_muted())
                                        .child(tr!(
                                            "settings",
                                            "about.publishedAt",
                                            date = format_published_at(&release.published_at)
                                        )),
                                )
                                .child(
                                    ui::primary_button(
                                        "about-download",
                                        t("settings", "about.downloadFromGitHub"),
                                    )
                                    .w_full()
                                    .py(px(8.0))
                                    .on_click(move |_, _window, cx: &mut App| {
                                        cx.open_url(&release.url);
                                    }),
                                )
                        } else {
                            div()
                                .text_size(ui::font_px(13.0))
                                .text_color(ui::text_secondary())
                                .child(t("settings", "about.upToDate"))
                        }),
                )
            })
            .child(ui::hint(t("settings", "about.footer")))
            .into_any_element()
    }
}

// ─── 小件 ─────────────────────────────────────────────────────

/// 默认项的单选圆点(shell / 编辑器列表共用)。`w-3 h-3 rounded-full border-2`。
fn radio_dot(id: String, selected: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .w(px(12.0))
        .h(px(12.0))
        .flex_none()
        .rounded_full()
        .border_2()
        .cursor_pointer()
        .border_color(if selected {
            ui::accent()
        } else {
            ui::border_strong()
        })
        .when(selected, |el| el.bg(ui::accent()))
}

/// 「+ 添加…」的虚线按钮。gpui 没有虚线边框,用 accent 淡底 + 实线描边近似。
fn dashed_button(id: &'static str, label: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .w_full()
        .flex()
        .justify_center()
        .py(px(10.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(ui::border_default())
        .cursor_pointer()
        .text_size(ui::font_px(13.0))
        .text_color(ui::text_muted())
        .hover(|el| el.border_color(ui::accent()).text_color(ui::accent()))
        .child(label)
}

/// 行内编辑表单的外壳。新增态用 accent 描边(原版是 accent 虚线)。
fn form_card(adding: bool) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .p(px(12.0))
        .rounded(px(6.0))
        .bg(ui::bg_base())
        .border_1()
        .border_color(if adding {
            ui::accent()
        } else {
            ui::border_default()
        })
}

/// 字体输入框:上标签下整宽输入框(原版 `FontFamilyInput`)。
fn font_family_input(label: &'static str, input: &Entity<InputState>) -> gpui::Div {
    div()
        .flex()
        .flex_col()
        .gap(px(6.0))
        .child(
            div()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_primary())
                .child(label),
        )
        .child(Input::new(input))
}

/// 单选段容器(`flex gap-2`)。
fn choice_group() -> gpui::Div {
    div().flex().gap(px(8.0))
}

/// 快捷键页的一行:左描述、右键帽。
fn shortcut_row(desc: &'static str, keys: String) -> gpui::Div {
    ui::settings_card()
        .flex()
        .items_center()
        .justify_between()
        .gap(px(16.0))
        .child(
            div()
                .text_size(ui::font_px(13.0))
                .text_color(ui::text_primary())
                .child(desc),
        )
        .child(ui::kbd(keys))
}

/// 一条带色描边的提示条(错误 / 成功 / 警告共用)。
fn banner(text: String, color: Hsla) -> gpui::Div {
    div()
        .px(px(12.0))
        .py(px(8.0))
        .rounded(px(4.0))
        .border_1()
        .border_color(color)
        .text_size(ui::font_px(11.0))
        .text_color(color)
        .children(
            text.split('\n')
                .map(|line| div().child(line.to_string()))
                .collect::<Vec<_>>(),
        )
}

/// 皮肤预览里的小横杠。
fn mini_bar(width: f32, color: Hsla, alpha: f32) -> gpui::Div {
    div()
        .h(px(4.0))
        .w(px(width))
        .rounded_full()
        .bg(ui::with_alpha(color, alpha))
}

/// 配置片段里的文件名行(带 `(note)`)。
fn snippet_file_name(file: &str, note: Option<&str>) -> gpui::Div {
    let text = match note {
        Some(note) if !note.is_empty() => format!("{file} ({note})"),
        _ => file.to_string(),
    };
    div()
        .mb(px(4.0))
        .text_color(ui::text_secondary())
        .child(text)
}

/// 配置片段正文。gpui 的文本不认 `\n`,拆成一行一个 child。
fn snippet_lines(content: &str) -> Vec<gpui::Div> {
    content
        .split('\n')
        .map(|line| {
            div().child(if line.is_empty() {
                SharedString::from(" ")
            } else {
                SharedString::from(line.to_string())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 分页 id 与原版**一字不差** —— 改了会让外部深链(`initialPage`)失效。
    #[test]
    fn 分页_id_与原版一致() {
        let ids: Vec<&str> = ALL_PAGES.iter().map(|p| p.id()).collect();
        assert_eq!(
            ids,
            vec![
                "terminal",
                "clipboard",
                "appearance",
                "font",
                "ai-notification",
                "ai-hook",
                "system",
                "editor",
                "shortcuts",
                "about",
            ]
        );
        // 反查也要通
        for page in ALL_PAGES {
            assert_eq!(SettingsPage::from_id(page.id()), Some(*page));
        }
        assert_eq!(SettingsPage::from_id("nope"), None);
    }

    /// 侧栏分组覆盖全部分页,且顺序与扁平序列一致(↑↓ 导航按后者走)。
    #[test]
    fn 侧栏分组覆盖全部分页() {
        let flat: Vec<SettingsPage> = MENU_GROUPS
            .iter()
            .flat_map(|(_, pages)| pages.iter().copied())
            .collect();
        assert_eq!(flat, ALL_PAGES.to_vec());
    }

    /// `selectionAutoCopySecs` 的四个分支:0 / <0.2 / >60 / 非法。
    #[test]
    fn 停留时长归一的四个分支() {
        // 0 = 关掉该功能(唯一出口),**不能**被钳成 0.2
        assert_eq!(normalize_number(NumField::Dwell, "0"), Some(0.0));
        assert_eq!(normalize_number(NumField::Dwell, "0.05"), Some(0.2));
        assert_eq!(normalize_number(NumField::Dwell, "999"), Some(60.0));
        assert_eq!(normalize_number(NumField::Dwell, "abc"), None);
        assert_eq!(normalize_number(NumField::Dwell, "-1"), None);
        assert_eq!(normalize_number(NumField::Dwell, ""), None);
        // 合法区间内原样保留(小数不被截尾)
        assert_eq!(normalize_number(NumField::Dwell, "1.5"), Some(1.5));
    }

    /// 整数项:低于 min 一律无效(回落已保存值),高于 max 钳到 max,小数截尾。
    #[test]
    fn 整数行归一() {
        assert_eq!(normalize_number(NumField::TrayMax, "0"), None);
        assert_eq!(normalize_number(NumField::TrayMax, "1"), Some(1.0));
        assert_eq!(normalize_number(NumField::TrayMax, "99"), Some(20.0));
        assert_eq!(normalize_number(NumField::TrayMax, "3.9"), Some(3.0));
        assert_eq!(normalize_number(NumField::LineThreshold, "-1"), None);
        assert_eq!(normalize_number(NumField::LineThreshold, "0"), Some(0.0));
        assert_eq!(
            normalize_number(NumField::Scrollback, "999999"),
            Some(MAX_SCROLLBACK as f64)
        );
        assert_eq!(normalize_number(NumField::Scrollback, "nan"), None);
    }

    /// 数字回显:整数不带小数点,浮点保留必要的小数。
    #[test]
    fn 数字回显() {
        assert_eq!(number_text(NumField::Scrollback, 10000.0), "10000");
        assert_eq!(number_text(NumField::Dwell, 1.0), "1");
        assert_eq!(number_text(NumField::Dwell, 1.5), "1.5");
    }

    /// 远程粘贴目录:空串回落默认(不落空串让后端每次兜底)。
    #[test]
    fn 远程粘贴目录归一() {
        let default = mt_config::default_remote_paste_dir();
        assert_eq!(normalize_remote_paste_dir(""), default);
        assert_eq!(normalize_remote_paste_dir("   "), default);
        assert_eq!(normalize_remote_paste_dir(" /tmp/x "), "/tmp/x");
        // `..` 的拒绝在后端,这里不重复判(两处判定会漂移)
        assert_eq!(normalize_remote_paste_dir("../x"), "../x");
    }

    /// 外观三字段联动:激活外置皮肤时主题/皮肤两段**全不高亮**。
    #[test]
    fn 皮肤激活时单选段全不选中() {
        assert_eq!(choice_value(None, "dark"), "dark");
        assert_eq!(choice_value(None, "none"), "none");
        assert_eq!(choice_value(Some("neon"), "dark"), "");
        assert_eq!(choice_value(Some("neon"), "none"), "");
    }

    fn reg(agent: &str, registered: usize, total: usize) -> HookRegistrationInfo {
        HookRegistrationInfo {
            agent: agent.into(),
            label: agent.into(),
            file: String::new(),
            registered,
            total,
        }
    }

    /// hook 页默认勾选:装过的只勾那几家;一家都没装过才全勾。
    #[test]
    fn hook_默认勾选() {
        let list = vec![reg("claude", 16, 16), reg("codex", 0, 8), reg("grok", 0, 6)];
        assert_eq!(default_selected_agents(&list), vec!["claude".to_string()]);

        let none = vec![reg("claude", 0, 16), reg("codex", 0, 8), reg("grok", 0, 6)];
        assert_eq!(
            default_selected_agents(&none),
            vec!["claude".to_string(), "codex".to_string(), "grok".to_string()]
        );

        // 旧事件集(registered < total)也算「装过」
        let stale = vec![reg("claude", 3, 16), reg("codex", 0, 8), reg("grok", 0, 6)];
        assert_eq!(default_selected_agents(&stale), vec!["claude".to_string()]);

        assert!(default_selected_agents(&[]).is_empty());
    }

    /// system 页托盘子项:总开关关掉时**不渲染**(而不是置灰)。
    #[test]
    fn 托盘子项在总开关关闭时不渲染() {
        assert!(tray_children_visible(true));
        assert!(!tray_children_visible(false));
    }

    /// 版本比较:去 `v` 前缀、缺段按 0。
    #[test]
    fn 版本比较() {
        use std::cmp::Ordering;
        assert_eq!(compare_versions("v0.13.2", "0.13.1"), Ordering::Greater);
        assert_eq!(compare_versions("0.13.1", "v0.13.1"), Ordering::Equal);
        assert_eq!(compare_versions("0.14", "0.13.9"), Ordering::Greater);
        // 缺段按 0:`1.0` == `1.0.0`
        assert_eq!(compare_versions("1.0", "1.0.0"), Ordering::Equal);
        assert_eq!(compare_versions("0.9.9", "0.10.0"), Ordering::Less);
        // 带后缀的段取前导数字
        assert_eq!(compare_versions("1.2.3-beta", "1.2.3"), Ordering::Equal);
    }

    /// 发布日期:ISO → `2026/8/19`(locale 写死,与原版一致)。
    #[test]
    fn 发布日期格式() {
        assert_eq!(format_published_at("2026-08-19T03:21:00Z"), "2026/8/19");
        assert_eq!(format_published_at("2026-12-01"), "2026/12/1");
        // 认不出的原样返回,不崩
        assert_eq!(format_published_at("nope"), "nope");
    }
}
