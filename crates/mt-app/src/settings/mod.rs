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
//! # 外观页只剩两档皮肤
//!
//! 原版的「皮肤」单选段(none / blueprint / fluent2)**整段已移除**:GPUI 侧
//! 从来没有内置皮肤色表,那一栏长期是「无」可选、另两项置灰,留着只是噪声。
//! 现在的口径是**默认皮肤**(主题段的深色 / 浅色 / 跟随系统)与**外置皮肤**
//! (用户自己导入的主题包卡片)两档,`AppConfig` 的 `skin` 字段随之删除。
//!
//! # 无消费方的设置项
//!
//! 长文本粘贴三项、远程粘贴目录、托盘三项、智能 Ctrl+C/V —— 字段都已在磁盘格式里,
//! UI 照原版做出来,但 GPUI 侧还没有消费方(分别属 audit #30 / #28 / #21 与
//! 终端剪贴板批)。改了会落盘、重启后还在,只是暂时没有效果。
//!
//! # 目录划分
//!
//! 本模块只留**面板本体**:分页枚举与侧栏分组、[`SettingsView`] 的状态字段、
//! 草稿行的归一与提交、`open_settings` 与分页路由。十个分页各自的渲染按主题
//! 分在四个 `pages_*` 里,共用小件在 `widgets`:
//!
//! | 文件 | 内容 |
//! |------|------|
//! | `pages_terminal` | terminal(Shell 列表)/ clipboard |
//! | `pages_appearance` | appearance(语言 / 主题 / 外置皮肤)/ font |
//! | `pages_ai` | ai-notification / ai-hook |
//! | `pages_system` | system / editor / shortcuts / about |
//! | `widgets` | 页/节骨架、设置行原语与零碎视图件 |
//!
//! about 页要用的版本比较与 GitHub 查询**不在这棵树里** —— 它同时被 `main.rs`
//! 的启动自检引用,住在顶层 [`crate::update_check`]。

use gpui::{
    AnyElement, App, AppContext, Context, Entity, FocusHandle, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, SharedString, StatefulInteractiveElement, Styled,
    Subscription, Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::input::{InputEvent, InputState};
use mt_ai::hook_registry::HookRegistrationInfo;

use crate::i18n::t;
use crate::prompt::{kind, open_guarded};
use crate::store::{AppStore, MAX_SCROLLBACK, resolve_scrollback};
use crate::ui;
use crate::update_check::ReleaseInfo;

mod pages_ai;
mod pages_appearance;
mod pages_system;
mod pages_terminal;
mod widgets;

use pages_appearance::ThemeCard;

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

// ─── 两字段联动(外观页,纯函数) ──────────────────────────────

/// 主题单选段的当前选中值。
///
/// **激活外置皮肤时返回空串** —— 三个按钮全不高亮(`SettingsModal.tsx:827`
/// 的 `config.customThemeId ? '' : config.theme`)。这不是 bug:外置皮肤
/// 不是 dark/light/auto 里的任何一个,高亮谁都是撒谎。
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

    // ── system 页:本地下载目录 ──
    download_dir_busy: bool,
    download_dir_error: Option<String>,
    download_dir_validation_key: Option<String>,

    // ── about 页 ──
    checking: bool,
    latest: Option<ReleaseInfo>,
    update_error: Option<String>,

    /// 后台任务(hook 动作 / 皮肤导入 / 检查更新)。换一次就丢掉上一次。
    _job: Option<Task<()>>,
    /// 下载目录选择与校验独立持有，避免覆盖其它设置页正在运行的任务。
    _download_dir_job: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

/// 面板头部高度(含 1px 下边框)。正文高度按它扣,见 [`render_dialog_header`]。
const HEADER_H: f32 = 52.0;

/// 面板头部:标题 + ✕。逐条对照原版 `Modal.tsx:186-194` 的 header
/// (`px-5 py-4` + 下边框 + `text-lg font-semibold` 标题 + `ModalCloseButton`)。
///
/// **不用 `Dialog::title`**:设置面板要 `p_0()`(左右两列自己贴边铺满),而
/// `Dialog` 的标题内边距跟着同一个 padding 走 —— 一 `p_0()` 标题就贴死在面板
/// 左上角的圆角上,而且没有分隔线。它自带的 ✕ 画 `IconName::Close`
/// (0.5.1 无 svg 资产 → 空白),同样只能自绘。
fn render_dialog_header() -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .h(px(HEADER_H))
        .items_center()
        .justify_between()
        .gap(px(12.0))
        .px(px(20.0))
        .border_b_1()
        .border_color(ui::border_subtle())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(ui::font_px(15.0))
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(ui::text_primary())
                .child(t("settings", "title")),
        )
        .child(
            div()
                .id("settings-close")
                .flex_none()
                .w(px(28.0))
                .h(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .text_size(ui::font_px(15.0))
                .text_color(ui::text_muted())
                .cursor_pointer()
                .hover(|el| el.bg(ui::border_subtle()).text_color(ui::text_primary()))
                .child("✕")
                // 程序化关闭必须走 `close_guarded`:`window.close_dialog` 不触发
                // `Dialog::on_close`,覆盖物栈里的登记摘不掉就再也开不出设置面板
                .on_click(|_, window, cx| {
                    crate::prompt::close_guarded(kind::SETTINGS, window, cx);
                }),
        )
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
            // 原版 `w-[680px] max-h-[80vh]`(`SettingsModal.tsx:2099`)。两条都要
            // 按视口现算:`Dialog` 的宽是定值、位置按「视口中心 − 宽/2」推,
            // 窗口比它窄时面板两侧一起出界(原版那边 flex-shrink 会把它压回来)。
            // 宽度这条比原版更进一步:680 退居**下限**,常规取 60vw —— 这一页
            // 是「窄侧栏 + 右侧一长串设置行」,定值宽在大屏上右列被挤成一小条
            let viewport = window.viewport_size();
            let width = ui::ratio_dialog_width(px(680.0), 0.6, viewport);
            // 原版没有 px 上限(只有 `max-h-[80vh]`),preferred 给视口高
            // 等于只按 80vh 钳 —— 面板总高与原版内容撑满时一致
            let body = ui::clamp_dialog_body_height(viewport.height, viewport, 0.8, px(HEADER_H));
            dialog
                .w(width)
                .p_0()
                // 头部自绘(见 [`render_dialog_header`]):`p_0()` 之下 `Dialog`
                // 自带的 title 内边距是 0,标题会贴死在面板左上角圆角上;
                // 它自带的 ✕ 画的是 `IconName::Close`,0.5.1 无 svg 资产 →
                // 渲染成空白,留着等于在右上角埋一个看不见的可点区
                .close_button(false)
                // 改了半天设置、误点遮罩就没了 —— 面板里还有编辑中的表单
                .overlay_closable(false)
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .child(render_dialog_header())
                        .child(div().h(body).child(view.clone())),
                )
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
            download_dir_busy: false,
            download_dir_error: None,
            download_dir_validation_key: None,
            checking: false,
            latest: None,
            update_error: None,
            _job: None,
            _download_dir_job: None,
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
                    // ⚠️ 少了 `min_w_0()` 这一列会**撑破面板**:taffy 按 CSS 的
                    // 「flex 子项 min-width:auto = min-content」给它兜底,而 gpui
                    // 量 min-content 时不给换行宽度(`elements/text.rs:347`:
                    // `AvailableSpace::MinContent` → `wrap_width = None`),于是最长
                    // 的那条中文说明(回滚行数 desc)整行不折,成了这一列的宽度下限。
                    // 列宽越过 680 的面板后被 `overflow_hidden` 裁掉 —— 用户看到的
                    // 就是「卡片 / 按钮 / 说明文字被右缘切断」。
                    //
                    // 原版没有这条:`overflow-y-auto` 让 overflow-x 的计算值变成
                    // auto,`min-width:auto` 随之解析为 0,列被压回可用宽、文字换行
                    // (`SettingsModal.tsx:2166` 那一列就是 `flex-1 overflow-y-auto`)。
                    // taffy 只按本轴的 overflow 判(`compute/flexbox.rs:791`),
                    // 不做 CSS 那个跨轴的 visible→auto 修正,所以得显式写。
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .px(px(20.0))
                    .py(px(16.0))
                    .child(self.render_page(cx)),
            )
    }
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

    /// 外观页联动:激活外置皮肤时主题段**三个按钮全不高亮**。
    #[test]
    fn 皮肤激活时主题段不选中() {
        assert_eq!(choice_value(None, "dark"), "dark");
        assert_eq!(choice_value(None, "auto"), "auto");
        assert_eq!(choice_value(Some("neon"), "dark"), "");
        assert_eq!(choice_value(Some("neon"), "auto"), "");
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
}
