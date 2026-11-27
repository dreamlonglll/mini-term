//! 设置面板:两级侧栏 + 13 个分页。逐页对照 `src/components/SettingsModal.tsx`
//! (「环境变量」「远程通知」「扩展」三页是 GPUI 版新增的,原版没有)。
//!
//! ```text
//! ┌──────────────┬────────────────────────────────────────┐
//! │ 终端          │                                        │
//! │  · Shell      │   (当前分页)                            │
//! │  · 复制粘贴    │                                        │
//! │  · 环境变量    │                                        │
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
//!    需要额外副作用的(主题 / 字号 / 字族 / 回滚行数 / 停留时长 / 帧率)各有专用 setter;
//! 2. **数字行是草稿态**:输入期间只改草稿,失焦 / 回车才归一并提交 ——
//!    边打字边 clamp 会让「1000」在敲到「1」时就被吃掉
//!    (`SettingsModal.tsx:167-171` 的注释)。关面板时兜底再提交一次(点 ✕ / Esc 不保证
//!    输入框先失焦)。滑块相反,**拖动即时提交**;
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
//! 草稿行的归一与提交、`open_settings` 与分页路由。十二个分页各自的渲染按主题
//! 分在七个 `pages_*` 里,共用小件在 `widgets`:
//!
//! | 文件 | 内容 |
//! |------|------|
//! | `pages_terminal` | terminal(Shell 列表)/ clipboard |
//! | `pages_env_vars` | env-vars(全局终端环境变量,表格与项目弹窗共用 `crate::env_vars`) |
//! | `pages_appearance` | appearance(语言 / 主题 / 外置皮肤)/ font |
//! | `pages_ai` | ai-notification / ai-hook |
//! | `pages_remote_notify` | remote-notify(Bark / Telegram / Webhook 远程通知,发送在 `crate::remote_notify`) |
//! | `pages_system` | system / editor / about |
//! | `pages_extensions` | extensions(按需扩展:远程 Git helper 等的下载 / 删除 / 离线导入 / 下载源;状态与下载在 `crate::extensions`) |
//! | `pages_shortcuts` | shortcuts(键位表、录制改键、冲突与就地提示;键位逻辑在 `crate::hotkeys`) |
//! | `widgets` | 页/节骨架、设置行原语与零碎视图件 |
//!
//! about 页要用的版本比较与 GitHub 查询**不在这棵树里** —— 它同时被 `main.rs`
//! 的启动自检引用,住在顶层 [`crate::update_check`]。
//!
//! # 切页动画
//!
//! 导航是竖排的:往下面那一页切,新页从内容列底边推入、旧页向上推出,反向镜像
//! (`motion_kit::push_pair`,[`mt_ui::motion::PANEL_SWAP`])。push 层包在内容列的滚动
//! 容器**外面**,新旧两页各是一个滚动容器(出场层停在离开时的滚动位置;新页从顶上看起)。
//! 导航的选中底色 + 左侧竖条是一块滑块(`segment_slider`),导航里夹着分组标题、各项的
//! y 不规则,所以先量再滑([`mt_ui::motion::TAB_INDICATOR`])。

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, FocusHandle, Focusable as _,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Task, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::input::{InputEvent, InputState};
use mt_ai::hook_registry::HookRegistrationInfo;

use crate::dialog_host::Dialog;
use crate::env_vars::EnvRow;
use crate::i18n::t;
use crate::motion_kit::{KeyedSwap, PushAxis, push_pair};
use crate::prompt::{kind, open_guarded_with_dismiss};
use crate::segment_slider::IndicatorSlider;
use crate::store::{AppStore, MAX_SCROLLBACK, resolve_scrollback};
use crate::ui;
use crate::update_check::ReleaseInfo;

mod pages_ai;
mod pages_appearance;
mod pages_env_vars;
mod pages_extensions;
mod pages_remote_notify;
mod pages_shortcuts;
mod pages_system;
mod pages_terminal;
mod widgets;

use pages_appearance::{ThemeCard, ThemeThumbKey, release_theme_thumbs};
use pages_extensions::ExtensionsNotice;
use pages_remote_notify::RemoteNotifyForm;
use pages_shortcuts::ShortcutsState;

// ─── 分页 ─────────────────────────────────────────────────────

/// 设置分页。
///
/// ⚠️ [`Self::id`] 的字符串**与原版一字不差**(`SettingsModal.tsx:40-50`)——
/// 原版注释明说「旧 id 一律保留、拆页只挪内容不改 key」,因为外部深链
/// (`initialPage`)会因为改名失效。GPUI 版新增的页(`env-vars`)同样一经发布不改。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SettingsPage {
    Terminal,
    Clipboard,
    /// 全局终端环境变量。GPUI 版新增,原版没有这一页。
    EnvVars,
    Appearance,
    Font,
    AiNotification,
    /// 远程通知(issue #87)。GPUI 版新增,原版没有这一页。
    RemoteNotify,
    AiHook,
    System,
    Editor,
    /// 按需扩展(远程 Git helper 等)。GPUI 版新增,原版没有这一页。
    Extensions,
    Shortcuts,
    About,
}

impl SettingsPage {
    /// 深链 id。与原版联合类型的字面量逐条对齐。
    pub fn id(self) -> &'static str {
        match self {
            Self::Terminal => "terminal",
            Self::Clipboard => "clipboard",
            Self::EnvVars => "env-vars",
            Self::Appearance => "appearance",
            Self::Font => "font",
            Self::AiNotification => "ai-notification",
            Self::RemoteNotify => "remote-notify",
            Self::AiHook => "ai-hook",
            Self::System => "system",
            Self::Editor => "editor",
            Self::Extensions => "extensions",
            Self::Shortcuts => "shortcuts",
            Self::About => "about",
        }
    }

    /// 侧栏标签的 i18n key(`settings` 命名空间内的相对 key)。
    fn label_key(self) -> &'static str {
        match self {
            Self::Terminal => "menu.shell",
            Self::Clipboard => "menu.clipboard",
            Self::EnvVars => "menu.envVars",
            Self::Appearance => "menu.appearance",
            Self::Font => "menu.font",
            Self::AiNotification => "menu.aiNotification",
            Self::RemoteNotify => "menu.remoteNotify",
            Self::AiHook => "menu.aiHook",
            Self::System => "menu.general",
            Self::Editor => "menu.editor",
            Self::Extensions => "menu.extensions",
            Self::Shortcuts => "menu.shortcuts",
            Self::About => "menu.about",
        }
    }

    /// 按深链 id 反查分页。眼下的深链入口(首屏引导、Hook 提示 toast)直接传枚举,
    /// 只有单测在用;留着是给将来从字符串来的入口(移动端、命令行参数)。
    #[cfg(test)]
    pub fn from_id(id: &str) -> Option<Self> {
        ALL_PAGES.iter().copied().find(|p| p.id() == id)
    }
}

/// 侧栏分组。空标题 = 一条分隔线(`SettingsModal.tsx:2059-2065`)。
const MENU_GROUPS: &[(&str, &[SettingsPage])] = &[
    (
        "menu.groupTerminal",
        &[
            SettingsPage::Terminal,
            SettingsPage::Clipboard,
            SettingsPage::EnvVars,
        ],
    ),
    (
        "menu.groupAppearance",
        &[SettingsPage::Appearance, SettingsPage::Font],
    ),
    (
        "menu.groupAi",
        &[
            SettingsPage::AiNotification,
            SettingsPage::RemoteNotify,
            SettingsPage::AiHook,
        ],
    ),
    (
        "menu.groupSystem",
        &[
            SettingsPage::System,
            SettingsPage::Editor,
            SettingsPage::Extensions,
        ],
    ),
    ("", &[SettingsPage::Shortcuts, SettingsPage::About]),
];

/// 扁平化后的分页序列 —— ↑↓ 在它上面环形移动,跳过分组标题
/// (`SettingsModal.tsx:2069` 的 `MENU_ITEMS`)。
pub const ALL_PAGES: &[SettingsPage] = &[
    SettingsPage::Terminal,
    SettingsPage::Clipboard,
    SettingsPage::EnvVars,
    SettingsPage::Appearance,
    SettingsPage::Font,
    SettingsPage::AiNotification,
    SettingsPage::RemoteNotify,
    SettingsPage::AiHook,
    SettingsPage::System,
    SettingsPage::Editor,
    SettingsPage::Extensions,
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
    /// 前台帧率(`terminalFpsForeground`)
    FpsForeground,
    /// 后台帧率(`terminalFpsBackground`)
    FpsBackground,
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
            Self::FpsForeground => fps_bounds(&crate::redraw::FOREGROUND_FPS),
            Self::FpsBackground => fps_bounds(&crate::redraw::BACKGROUND_FPS),
        }
    }
}

/// 帧率两行的区间直接取节拍器那边的常量,两处不各写一份。
fn fps_bounds(range: &std::ops::RangeInclusive<u32>) -> (f64, f64) {
    (f64::from(*range.start()), f64::from(*range.end()))
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
    TerminalCjkFontFamily,
    /// 扩展下载地址模板(扩展页)。不合规的不落盘,页上就地标红。
    ExtensionsDownloadUrl,
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

/// 面板里按 Esc(弹窗的 `Cancel`)这一下退哪一层。由里往外一次退一层,最外一层才关面板:
/// 二级表单(编辑 / 新建 shell、编辑器)里有没保存的内容,一下 Esc 连面板一起丢太重。
///
/// **只看当前这一页**:表单是按页挂着的,别的页上留着一份没收的表单时,在这一页按 Esc
/// 去收一份看不见的表单等于「按了没反应」—— 那份照旧留着,回到那一页还在。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsEsc {
    /// 删除 shell 的就地二次确认挂着:撤掉确认(与命令库的行内删除确认同一口径)。
    CancelShellDelete,
    /// 收起 shell 表单,退回列表。
    CloseShellForm,
    /// 收起编辑器表单,退回列表。
    CloseEditorForm,
    /// 关面板。
    CloseDialog,
}

fn settings_esc(
    page: SettingsPage,
    shell_delete_pending: bool,
    shell_form_open: bool,
    editor_form_open: bool,
) -> SettingsEsc {
    match page {
        SettingsPage::Terminal if shell_delete_pending => SettingsEsc::CancelShellDelete,
        SettingsPage::Terminal if shell_form_open => SettingsEsc::CloseShellForm,
        SettingsPage::Editor if editor_form_open => SettingsEsc::CloseEditorForm,
        _ => SettingsEsc::CloseDialog,
    }
}

pub struct SettingsView {
    store: Entity<AppStore>,
    page: SettingsPage,
    focus: FocusHandle,
    /// 切页的整幅 push(见模块注释「切页动画」)。
    page_swap: KeyedSwap<SettingsPage>,
    /// 键盘 ↑/↓ 环形切页时的显式方向(防止首尾回绕时按索引大小判断反向倒车)。
    pending_page_dir: Option<f32>,
    /// 内容列的滚动位置(换页回到顶上)。
    page_scroll: ScrollHandle,
    /// 切页出场层的滚动位置:换页那一刻从 `page_scroll` 抄过来,旧页停在离开时的位置滑走。
    exit_scroll: ScrollHandle,
    /// 导航的滑动选中块。
    nav_slider: IndicatorSlider,
    /// 外观页「语言」段控件的滑动选中块。
    lang_slider: IndicatorSlider,

    // ── terminal 页:shell 列表 ──
    shell_editing: Editing,
    /// 删除的就地二次确认:点了「删除」的那一行 `(行号, shell 名)`,该行的按钮换成
    /// 「确认删除 / 取消」,再点一下才真删(见 `pages_terminal`)。记名字是为了确认那一刻核对
    /// 行号还指着同一个 shell(名字不保证唯一,所以两样都记)。
    shell_pending_delete: Option<(usize, String)>,
    shell_name: Entity<InputState>,
    shell_command: Entity<InputState>,
    shell_args: Entity<InputState>,
    shell_error: Option<&'static str>,

    // ── env-vars 页:全局终端环境变量 ──
    env_rows: Vec<EnvRow>,
    env_next_rid: u64,
    /// 每行两个输入框的订阅(失焦 / 回车即提交),删行时随行一起丢。
    env_subs: HashMap<u64, [Subscription; 2]>,

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
    num_fps_foreground: Entity<InputState>,
    num_fps_background: Entity<InputState>,

    // ── 文本行(草稿态)──
    txt_remote_paste_dir: Entity<InputState>,
    txt_ui_font: Entity<InputState>,
    txt_terminal_font: Entity<InputState>,
    txt_terminal_cjk_font: Entity<InputState>,

    // ── appearance 页:外置皮肤 ──
    theme_cards: Vec<ThemeCard>,
    /// 向 gpui 资源系统要过的卡片缩略图(见 `pages_appearance::ThemeThumbnail`)。
    /// 资源缓存与图集纹理都是进程级、不自动淘汰的,离开外观页 / 列表变了由
    /// `release_stale_theme_thumbs` 放,关掉设置由 `on_release` 全放。
    theme_thumbs: HashSet<ThemeThumbKey>,
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

    // ── remote-notify 页 ──
    remote: RemoteNotifyForm,

    // ── shortcuts 页:录制改键(见 `pages_shortcuts`)──
    shortcuts: ShortcutsState,

    // ── extensions 页 ──
    txt_extensions_url: Entity<InputState>,
    /// 导入结果 / 删除失败等页内提示。
    ext_notice: Option<ExtensionsNotice>,
    /// 删除 / 导入在后台跑着(两颗按钮置灰)。
    ext_busy: bool,
    /// 这次打开设置后扩展页的状态重 stat 过没有。
    ext_refreshed: bool,
    _ext_job: Option<Task<()>>,

    // ── system 页:本地下载目录 ──
    download_dir_busy: bool,
    download_dir_error: Option<String>,
    download_dir_validation_key: Option<String>,

    // ── about 页 ──
    checking: bool,
    latest: Option<ReleaseInfo>,
    /// `latest` 的发布说明,清洗过的 Markdown(查到时在后台清洗一次,不每帧重做)。
    latest_notes: SharedString,
    update_error: Option<String>,

    /// 后台任务(hook 动作 / 皮肤导入 / 检查更新)。换一次就丢掉上一次。
    _job: Option<Task<()>>,
    /// 下载目录选择与校验独立持有，避免覆盖其它设置页正在运行的任务。
    _download_dir_job: Option<Task<()>>,
    _subs: Vec<Subscription>,
    /// 关面板的收尾做过了没有(见 [`SettingsView::finish`]);`on_release` 只给没走关窗收尾的
    /// 路兜底。
    finished: bool,
}

/// 面板头部高度(含 1px 下边框)。正文高度按它扣,见 [`render_dialog_header`]。
const HEADER_H: f32 = 52.0;

/// 面板头部:标题 + ✕。逐条对照原版 `Modal.tsx:186-194` 的 header
/// (`px-5 py-4` + 下边框 + `text-lg font-semibold` 标题 + `ModalCloseButton`)。
///
/// **不用 `Dialog::title`**:设置面板要 `p_0()`(左右两列自己贴边铺满),而
/// `Dialog` 的标题内边距跟着同一个 padding 走 —— 一 `p_0()` 标题就贴死在面板
/// 左上角的圆角上,而且没有分隔线。它自带的 ✕ 是绝对定位在面板右上角的,`p_0()`
/// 之下落到 8,8、与这条自绘头部打架,同样只能自绘。(2026-09-19 补记:原先记的
/// 是「它自带的 ✕ 画 `IconName::Close`,0.5.1 无 svg 资产 → 空白」——
/// 入口已挂 `gpui_kit_assets::Assets`,该判据作废,版面这条不变。)
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

/// 本次运行里设置面板上次停在哪一页(关面板那一刻记下,见 [`SettingsView::finish`])。
/// **不落盘**:重启后从第一页开始,与原版一致;只省掉「同一次运行里来回改同一页」的那几下点击。
struct LastSettingsPage(SettingsPage);
impl gpui::Global for LastSettingsPage {}

/// 打开时落在哪一页:深链指定的优先,其次是这次运行里上次停留的那页,都没有就第一页。
fn resolve_initial_page(
    requested: Option<SettingsPage>,
    remembered: Option<SettingsPage>,
) -> SettingsPage {
    requested.or(remembered).unwrap_or(SettingsPage::Terminal)
}

/// 打开设置面板。`initial_page` 是深链入口:首屏引导与「注册 Hook」提示直达
/// [`SettingsPage::AiHook`];快捷键 / 齿轮按钮传 `None`,回到上次停留的那页
/// (见 [`resolve_initial_page`])。
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
    let remembered = cx.try_global::<LastSettingsPage>().map(|last| last.0);
    let page = resolve_initial_page(initial_page, remembered);
    let view = cx.new(|cx| SettingsView::new(store, Some(page), window, cx));
    let focus = view.read(cx).focus.clone();
    // 关窗收尾:关面板那一刻(退场开始时)提交草稿、记页,不等实体释放(见 `SettingsView::finish`)。
    // defer 出去:收尾哪天从面板自己的 update 里触发(面板里加一颗关闭),也不会重入同一实体
    let on_dismiss = {
        let view = view.clone();
        move |_window: &mut Window, cx: &mut App| {
            cx.defer(move |cx| view.update(cx, |view, cx| view.finish(cx)));
        }
    };

    let build = {
        let view = view.clone();
        move |dialog: Dialog, window: &mut Window, _cx: &mut App| {
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
                // 它自带的 ✕ 也绝对定位到 8,8,与自绘头部那颗重叠
                // (2026-09-19 补记:入口已挂 `gpui_kit_assets::Assets`,原先记的
                // 「0.5.1 无 svg 资产 → 空白,等于埋一个看不见的可点区」已作废)
                .close_button(false)
                // 改了半天设置、误点遮罩就没了 —— 面板里还有编辑中的表单
                .overlay_closable(false)
                // Esc 由里往外一次退一层(删除确认 → 编辑 / 新建表单 → 关面板,见
                // [`settings_esc`])。焦点在表单输入框里也走得到这里:输入框的 `Escape`
                // 以 `cx.propagate()` 收尾,gpui 接着派发弹窗的 `Cancel`
                .on_cancel({
                    let view = view.clone();
                    move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                        view.update(cx, |view, cx| view.handle_escape(window, cx))
                    }
                })
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .child(render_dialog_header())
                        .child(div().h(body).child(view.clone())),
                )
        }
    };
    open_guarded_with_dismiss(kind::SETTINGS, window, cx, build, on_dismiss);

    // Dialog 打开时会把焦点抢到自己面板上,↑↓ 导航要的焦点必须排在它后面
    window.defer(cx, move |window, cx| {
        window.focus(&focus, cx);
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
        let (foreground_fps, background_fps) = {
            let store = store.read(cx);
            (store.foreground_fps(), store.background_fps())
        };
        let num_fps_foreground = num(
            cx,
            window,
            NumField::FpsForeground,
            f64::from(foreground_fps),
        );
        let num_fps_background = num(
            cx,
            window,
            NumField::FpsBackground,
            f64::from(background_fps),
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
        let txt_terminal_cjk_font = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(TERMINAL_CJK_FONT_PLACEHOLDER)
                .default_value(config.terminal_cjk_font_family.clone().unwrap_or_default())
        });
        let txt_extensions_url = cx.new(|cx| {
            InputState::new(window, cx)
                // placeholder 就是默认地址本身(留空 = 用它)
                .placeholder(crate::extensions::DEFAULT_DOWNLOAD_TEMPLATE)
                .default_value(config.extensions_download_url.clone().unwrap_or_default())
        });
        let remote = RemoteNotifyForm::new(&config.remote_notify, window, cx);

        widgets::clear_knobs();
        let mut this = Self {
            store,
            page: initial_page.unwrap_or(SettingsPage::Terminal),
            focus: cx.focus_handle(),
            page_swap: KeyedSwap::new(mt_ui::motion::PANEL_SWAP),
            pending_page_dir: None,
            page_scroll: ScrollHandle::new(),
            exit_scroll: ScrollHandle::new(),
            nav_slider: IndicatorSlider::new(),
            lang_slider: IndicatorSlider::new(),
            shell_editing: None,
            shell_pending_delete: None,
            shell_name: ph(cx, window, t("settings", "terminal.newNamePlaceholder")),
            shell_command: ph(cx, window, t("settings", "terminal.newCommandPlaceholder")),
            shell_args: ph(cx, window, t("settings", "terminal.newArgsPlaceholder")),
            shell_error: None,
            env_rows: Vec::new(),
            env_next_rid: 0,
            env_subs: HashMap::new(),
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
            num_fps_foreground,
            num_fps_background,
            txt_remote_paste_dir,
            txt_ui_font,
            txt_terminal_font,
            txt_terminal_cjk_font,
            theme_cards: Vec::new(),
            theme_thumbs: HashSet::new(),
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
            snippet_tab: mt_ai::hook_registry::HookAgent::Claude.key(),
            sound_warning: false,
            remote,
            shortcuts: ShortcutsState::default(),
            txt_extensions_url,
            ext_notice: None,
            ext_busy: false,
            ext_refreshed: false,
            _ext_job: None,
            download_dir_busy: false,
            download_dir_error: None,
            download_dir_validation_key: None,
            checking: false,
            latest: None,
            latest_notes: SharedString::default(),
            update_error: None,
            _job: None,
            _download_dir_job: None,
            _subs: Vec::new(),
            finished: false,
        };

        // 草稿行:失焦 / 回车才归一并提交(见模块注释第 2 条)。
        // 走 `subscribe_in` 而不是 `subscribe` —— 归一后要把值写回输入框,
        // 而 `InputState::set_value` 要 `&mut Window`。
        for (entity, field) in this.number_inputs() {
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
        for (entity, field) in this.text_inputs() {
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

        // 扩展页的状态在后台线程里变(下载进度 / 装好 / 失败),由下载服务的事件泵 notify 这个实体
        if let Some(hub) = crate::extensions::hub(cx) {
            this._subs.push(cx.observe(&hub, |_, _, cx| cx.notify()));
        }

        this.init_env_rows(&config.terminal_env_vars, window, cx);
        this.subscribe_remote_inputs(window, cx);

        // 关掉设置时把皮肤卡片的缩略图从资源缓存与图集里放掉(见字段注释)。
        // `on_release` 不在任何窗口的更新里,`drop_image` 自己遍历得到所有窗口。
        // 草稿提交与记页**不在这里做**:关面板那一刻由关窗收尾做掉(见 [`Self::finish`]),
        // 这里只给没走那条路的情形兜底
        cx.on_release(|this: &mut Self, cx: &mut App| {
            let keys: Vec<ThemeThumbKey> = this.theme_thumbs.drain().collect();
            release_theme_thumbs(&keys, cx, None);
            this.finish(cx);
        })
        .detach();

        this.refresh_theme_packs(cx);
        this.refresh_hook_state(cx);
        this
    }

    // ── 提交 ──

    /// 关面板的收尾(只做一次):草稿兜底提交(点 ✕ / Esc 关面板不保证输入框先失焦)、记住停在
    /// 哪一页(下次不指定分页打开时回到这一页,见 [`resolve_initial_page`])。
    ///
    /// 关窗那一刻调(`open_settings` 登记的关窗收尾),**不等实体释放**:弹窗外壳有退场动画,
    /// 实体要等它播完、层出栈才放,这期间再按 Ctrl+, 的话新面板会先按没提交的旧配置建好
    /// 输入框、落在上上次的那一页,旧实体随后释放时再提交 —— 新面板显示的是旧值。
    fn finish(&mut self, cx: &mut App) {
        if std::mem::replace(&mut self.finished, true) {
            return;
        }
        self.commit_drafts(cx);
        widgets::clear_knobs();
        cx.set_global(LastSettingsPage(self.page));
    }

    /// 数字草稿行。失焦 / 回车的订阅与关面板兜底共用这张表,新增一行两边都有。
    fn number_inputs(&self) -> [(Entity<InputState>, NumField); 7] {
        [
            (self.num_scrollback.clone(), NumField::Scrollback),
            (self.num_dwell.clone(), NumField::Dwell),
            (self.num_line_threshold.clone(), NumField::LineThreshold),
            (self.num_char_threshold.clone(), NumField::CharThreshold),
            (self.num_tray_max.clone(), NumField::TrayMax),
            (self.num_fps_foreground.clone(), NumField::FpsForeground),
            (self.num_fps_background.clone(), NumField::FpsBackground),
        ]
    }

    /// 文本草稿行,同 [`Self::number_inputs`]。
    fn text_inputs(&self) -> [(Entity<InputState>, TextField); 5] {
        [
            (self.txt_remote_paste_dir.clone(), TextField::RemotePasteDir),
            (self.txt_ui_font.clone(), TextField::UiFontFamily),
            (
                self.txt_terminal_font.clone(),
                TextField::TerminalFontFamily,
            ),
            (
                self.txt_terminal_cjk_font.clone(),
                TextField::TerminalCjkFontFamily,
            ),
            (
                self.txt_extensions_url.clone(),
                TextField::ExtensionsDownloadUrl,
            ),
        ]
    }

    /// 把还没提交的草稿全部落盘。收 `&mut App`:关面板时的兜底([`Self::finish`])也会从
    /// `on_release` 里调,那里只有 `App`,也写不回输入框 —— 面板都关了,不用写。没改的行
    /// 到 setter 都是空转。
    fn commit_drafts(&mut self, cx: &mut App) {
        for (input, field) in self.number_inputs() {
            self.save_number(field, &input, cx);
        }
        for (input, field) in self.text_inputs() {
            self.save_text(field, &input, cx);
        }
        self.commit_remote_drafts(cx);
        self.commit_env_vars(cx);
    }

    fn commit_number(
        &mut self,
        field: NumField,
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 归一后的值写回输入框(原版 `setDraft(String(next))`)
        let text = self.save_number(field, input, cx);
        if input.read(cx).value().as_ref() != text.as_str() {
            input.update(cx, |state, cx| state.set_value(text, window, cx));
        }
    }

    /// 归一并写进配置,返回归一后的显示串。不碰输入框,所以没有 `Window` 也能调。
    fn save_number(&self, field: NumField, input: &Entity<InputState>, cx: &mut App) -> String {
        let draft = input.read(cx).value().to_string();
        let saved = self.saved_number(field, cx);
        let next = normalize_number(field, &draft).unwrap_or(saved);
        let text = number_text(field, next);
        if (next - saved).abs() < f64::EPSILON {
            return text;
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
            NumField::FpsForeground => store.set_foreground_fps(next as u32, cx),
            NumField::FpsBackground => store.set_background_fps(next as u32, cx),
        });
        text
    }

    fn saved_number(&self, field: NumField, cx: &App) -> f64 {
        let store = self.store.read(cx);
        let config = store.config();
        match field {
            NumField::Scrollback => resolve_scrollback(config.terminal_scrollback as f64) as f64,
            NumField::Dwell => config.selection_auto_copy_secs.unwrap_or(1.0),
            NumField::LineThreshold => config.long_paste_line_threshold as f64,
            NumField::CharThreshold => config.long_paste_char_threshold as f64,
            NumField::TrayMax => config.tray_max_projects.unwrap_or(5) as f64,
            NumField::FpsForeground => f64::from(store.foreground_fps()),
            NumField::FpsBackground => f64::from(store.background_fps()),
        }
    }

    fn commit_text(
        &mut self,
        field: TextField,
        input: &Entity<InputState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = self.save_text(field, input, cx);
        if input.read(cx).value().as_ref() != text.as_str() {
            input.update(cx, |state, cx| state.set_value(text, window, cx));
        }
    }

    /// 归一并写进配置,返回归一后的文本。不碰输入框,同 [`Self::save_number`]。
    fn save_text(&self, field: TextField, input: &Entity<InputState>, cx: &mut App) -> String {
        let draft = input.read(cx).value().to_string();
        match field {
            TextField::RemotePasteDir => {
                let next = normalize_remote_paste_dir(&draft);
                if self.store.read(cx).config().remote_paste_dir != next {
                    let value = next.clone();
                    self.store.update(cx, |store, cx| {
                        store.patch_config(|c| c.remote_paste_dir = value, cx)
                    });
                }
                return next;
            }
            // 空串提交 → 写 `None` 而不是空字符串(`SettingsModal.tsx:881, 920`)
            TextField::UiFontFamily => {
                let next = Some(draft.clone()).filter(|s| !s.trim().is_empty());
                self.store
                    .update(cx, |store, cx| store.set_ui_font_family(next, cx));
            }
            TextField::TerminalFontFamily => {
                let next = Some(draft.clone()).filter(|s| !s.trim().is_empty());
                self.store
                    .update(cx, |store, cx| store.set_terminal_font_family(next, cx));
            }
            TextField::TerminalCjkFontFamily => {
                let next = Some(draft.clone()).filter(|s| !s.trim().is_empty());
                self.store
                    .update(cx, |store, cx| store.set_terminal_cjk_font_family(next, cx));
            }
            // 空串 = 默认地址;不合规的不落盘,输入框留着原文、页上标红(见扩展页)
            TextField::ExtensionsDownloadUrl => {
                let trimmed = draft.trim().to_string();
                if trimmed.is_empty() || crate::extensions::is_valid_template(&trimmed) {
                    let next = Some(trimmed.clone()).filter(|s| !s.is_empty());
                    self.store
                        .update(cx, |store, cx| store.set_extensions_download_url(next, cx));
                    return trimmed;
                }
            }
        }
        draft
    }

    // ── ↑↓ 导航 ──

    fn move_page(&mut self, delta: i32, cx: &mut Context<Self>) {
        let len = ALL_PAGES.len() as i32;
        let idx = ALL_PAGES.iter().position(|p| *p == self.page).unwrap_or(0) as i32;
        // 记住按键移动方向:末页按 ↓ 回绕到首页仍向下推入,首页按 ↑ 回绕到末页仍向上推入
        self.pending_page_dir = Some(if delta >= 0 { 1.0 } else { -1.0 });
        self.set_page(ALL_PAGES[(((idx + delta) % len + len) % len) as usize], cx);
    }

    /// 换分页的唯一入口。离开快捷键页时把那一页的录制(App 级的按键拦截器)、待选冲突、
    /// 就地提醒一并清掉 —— 拦截器绝不能跟着人跑到别的页去。
    fn set_page(&mut self, page: SettingsPage, cx: &mut Context<Self>) {
        if page != self.page {
            self.leave_shortcuts_page(cx);
            // 删除确认是就地的:人离开了那一页,确认态不跟着留(回来看到的是普通的「删除」)
            self.shell_pending_delete = None;
            // 出场层停在旧页离开时的位置;新页从顶上看起(同一个滚动容器换了内容,
            // 沿用旧页的偏移没有意义)
            self.exit_scroll.set_offset(self.page_scroll.offset());
            self.page_scroll.set_offset(gpui::Point::default());
        }
        self.page = page;
        cx.notify();
    }

    /// 弹窗的 `Cancel`(Esc)落到面板上:按 [`settings_esc`] 退一层。返回 `true` = 这一下关面板。
    fn handle_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match settings_esc(
            self.page,
            self.shell_pending_delete.is_some(),
            self.shell_editing.is_some(),
            self.editor_editing.is_some(),
        ) {
            SettingsEsc::CancelShellDelete => {
                self.shell_pending_delete = None;
                cx.notify();
                false
            }
            SettingsEsc::CloseShellForm => {
                self.close_shell_form(window, cx);
                false
            }
            SettingsEsc::CloseEditorForm => {
                self.close_editor_form(window, cx);
                false
            }
            SettingsEsc::CloseDialog => true,
        }
    }

    /// 键盘焦点若在 `inputs` 里,交回面板根(`self.focus`)。表单收起之前调:框一不画,焦点就
    /// 悬空在它身上 —— 悬空时按键从窗口根派发,↑/↓ 切页与 Esc 关面板一个都收不到。
    fn reclaim_focus_from(
        &self,
        inputs: &[&Entity<InputState>],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let in_form = inputs
            .iter()
            .any(|input| input.read(cx).focus_handle(cx).is_focused(window));
        if in_form {
            window.focus(&self.focus, cx);
        }
    }

    /// 收起 shell 表单、退回列表(「取消」与 Esc 共用)。
    fn close_shell_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reclaim_focus_from(
            &[&self.shell_name, &self.shell_command, &self.shell_args],
            window,
            cx,
        );
        self.shell_editing = None;
        cx.notify();
    }

    /// 收起编辑器表单、退回列表(「取消」与 Esc 共用)。
    fn close_editor_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reclaim_focus_from(&[&self.editor_name, &self.editor_command], window, cx);
        self.editor_editing = None;
        cx.notify();
    }
}

/// 切页的推移方向:往导航下面那一页切 = 新页从底边进(`> 0`),往上切反之。
fn page_order(old: SettingsPage, new: SettingsPage) -> f32 {
    let index = |page: SettingsPage| ALL_PAGES.iter().position(|p| *p == page).unwrap_or(0);
    if index(new) > index(old) { 1.0 } else { -1.0 }
}

/// 这次输入事件算不算「提交」(失焦 / 回车)。
fn commits(event: &InputEvent) -> bool {
    matches!(event, InputEvent::Blur | InputEvent::PressEnter { .. })
}

/// 终端字体输入框的 placeholder(`terminalCache.ts:50-51` 的
/// `DEFAULT_TERMINAL_FONT_FAMILY`)。
const DEFAULT_TERMINAL_FONT_PLACEHOLDER: &str =
    "'JetBrainsMono Nerd Font', 'CaskaydiaCove Nerd Font', 'JetBrains Mono', Consolas";

const TERMINAL_CJK_FONT_PLACEHOLDER: &str = "'Sarasa Fixed SC', 'Microsoft YaHei'";

// ─── 渲染 ─────────────────────────────────────────────────────

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        widgets::drive_knobs(window);
        // 切页:先报这一帧谁在台上,再取出场信息(见模块注释「切页动画」)
        let page = self.page;
        let explicit_dir = self.pending_page_dir.take();
        self.page_swap.observe(&page, |old, new| {
            explicit_dir.unwrap_or_else(|| page_order(*old, *new))
        });
        let swap = self.page_swap.frame(window);
        // 先对账再画:换走了外观页 / 皮肤列表变了,缩略图在这一帧放掉(外观页正作为出场层
        // 滑走时还在画,不放)
        let appearance_showing = page == SettingsPage::Appearance
            || swap.is_some_and(|(old, _, _)| old == SettingsPage::Appearance);
        self.release_stale_theme_thumbs(appearance_showing, window, cx);
        let current = self.page_column(page, "settings-page", self.page_scroll.clone(), window, cx);
        let body = match swap {
            Some((old, progress, dir)) => {
                let leaving = self.page_column(
                    old,
                    "settings-page-exit",
                    self.exit_scroll.clone(),
                    window,
                    cx,
                );
                push_pair(
                    current,
                    Some(leaving.into_any_element()),
                    progress,
                    PushAxis::Vertical,
                    dir,
                )
                .into_any_element()
            }
            None => current.into_any_element(),
        };
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
                    .child(body),
            )
    }
}

impl SettingsView {
    /// 内容列里的一页:滚动容器(`id` 区分在台上的与正在滑走的那一份,滚动位置各管各的)。
    fn page_column(
        &mut self,
        page: SettingsPage,
        id: &'static str,
        scroll: ScrollHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .size_full()
            .min_w_0()
            .overflow_y_scroll()
            .track_scroll(&scroll)
            .px(px(20.0))
            .py(px(16.0))
            .child(self.render_page(page, window, cx))
    }

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

        // 选中底色 + 左侧竖条是一块滑块(见模块注释「切页动画」),排在各项之前;还没量到
        // 位置那一帧由选中项自己画(与滑块落位后一模一样)
        let active_ix = ALL_PAGES.iter().position(|p| *p == self.page).unwrap_or(0);
        let ready = self.nav_slider.ready(active_ix);
        menu = menu.child(self.nav_slider.block(active_ix, |el| {
            el.flex()
                .items_center()
                .px(px(12.0))
                .rounded(px(4.0))
                .bg(ui::accent_subtle())
                .child(
                    div()
                        .w(px(2.0))
                        .h(px(16.0))
                        .flex_none()
                        .rounded(px(1.0))
                        .bg(ui::accent()),
                )
        }));
        let mut item_ix = 0usize;

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
                let probe = self.nav_slider.probe(item_ix);
                item_ix += 1;
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
                            el.text_color(ui::accent())
                                .when(!ready, |el| el.bg(ui::accent_subtle()))
                        })
                        .when(!active, |el| {
                            el.text_color(ui::text_secondary()).hover(|el| {
                                el.bg(ui::border_subtle()).text_color(ui::text_primary())
                            })
                        })
                        // 左侧激活竖条:**未选中时留位不留色** —— 切页时标签文字
                        // 不会横向抖一下(原版 :2150 的注释)。量到位置之后由滑块画
                        .child(
                            div()
                                .w(px(2.0))
                                .h(px(16.0))
                                .flex_none()
                                .rounded(px(1.0))
                                .when(active && !ready, |el| el.bg(ui::accent())),
                        )
                        .child(probe)
                        .child(
                            div()
                                .min_w(px(0.0))
                                .truncate()
                                .child(t("settings", page.label_key())),
                        )
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            this.set_page(page, cx);
                        })),
                );
            }
        }
        menu.into_any_element()
    }

    fn render_page(
        &mut self,
        page: SettingsPage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match page {
            SettingsPage::Terminal => self.render_terminal_page(cx),
            SettingsPage::Clipboard => self.render_clipboard_page(cx),
            SettingsPage::EnvVars => self.render_env_vars_page(cx),
            // 外观页要 `window`:皮肤卡片的缩略图走 `window.use_asset`
            SettingsPage::Appearance => self.render_appearance_page(window, cx),
            SettingsPage::Font => self.render_font_page(cx),
            SettingsPage::AiNotification => self.render_notification_page(cx),
            SettingsPage::RemoteNotify => self.render_remote_notify_page(cx),
            SettingsPage::AiHook => self.render_hook_page(cx),
            SettingsPage::System => self.render_system_page(cx),
            SettingsPage::Editor => self.render_editor_page(cx),
            SettingsPage::Extensions => self.render_extensions_page(cx),
            SettingsPage::Shortcuts => self.render_shortcuts_page(cx),
            SettingsPage::About => self.render_about_page(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Esc 由里往外一次退一层:删除确认 → 表单 → 关面板;只看当前这一页挂着的。
    #[test]
    fn esc先退表单再关面板() {
        use SettingsPage::{Editor, Font, Terminal};
        assert_eq!(
            settings_esc(Terminal, false, false, false),
            SettingsEsc::CloseDialog
        );
        assert_eq!(
            settings_esc(Terminal, false, true, false),
            SettingsEsc::CloseShellForm,
            "shell 表单开着:只退回列表"
        );
        assert_eq!(
            settings_esc(Terminal, true, true, false),
            SettingsEsc::CancelShellDelete,
            "删除确认最先撤(同命令库)"
        );
        assert_eq!(
            settings_esc(Editor, false, false, true),
            SettingsEsc::CloseEditorForm
        );
        // 别的页上留着的表单 / 确认态不归这一下 Esc 管
        assert_eq!(
            settings_esc(Editor, true, true, false),
            SettingsEsc::CloseDialog
        );
        assert_eq!(
            settings_esc(Terminal, false, false, true),
            SettingsEsc::CloseDialog
        );
        assert_eq!(settings_esc(Font, true, true, true), SettingsEsc::CloseDialog);
    }

    /// 切页方向跟导航的竖排次序:往下面那页切新页从底边进(`> 0`),往上切从顶上进。
    #[test]
    fn 切页方向跟导航次序() {
        assert_eq!(page_order(SettingsPage::Terminal, SettingsPage::Font), 1.0);
        assert_eq!(
            page_order(SettingsPage::About, SettingsPage::Terminal),
            -1.0
        );
        assert_eq!(
            page_order(SettingsPage::AiHook, SettingsPage::AiNotification),
            -1.0
        );
    }

    /// 分页 id 与原版**一字不差** —— 改了会让外部深链(`initialPage`)失效。
    /// `env-vars` 是 GPUI 版新增的,同一条规矩。
    #[test]
    fn 分页_id_与原版一致() {
        let ids: Vec<&str> = ALL_PAGES.iter().map(|p| p.id()).collect();
        assert_eq!(
            ids,
            vec![
                "terminal",
                "clipboard",
                "env-vars",
                "appearance",
                "font",
                "ai-notification",
                "remote-notify",
                "ai-hook",
                "system",
                "editor",
                "extensions",
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

    /// 打开时落在哪页:深链优先,其次本次运行上次停留的那页,都没有才是第一页。
    #[test]
    fn 打开时的分页取舍() {
        use SettingsPage::*;
        assert_eq!(resolve_initial_page(None, None), Terminal);
        assert_eq!(resolve_initial_page(None, Some(Font)), Font);
        assert_eq!(resolve_initial_page(Some(AiHook), Some(Font)), AiHook);
        assert_eq!(resolve_initial_page(Some(AiHook), None), AiHook);
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

    /// 帧率两行:区间取自节拍器那边的常量,低于下限回落已保存值,高于上限钳住。
    #[test]
    fn 帧率行归一() {
        let fg = |draft| normalize_number(NumField::FpsForeground, draft);
        let bg = |draft| normalize_number(NumField::FpsBackground, draft);
        assert_eq!(fg("60"), Some(60.0));
        assert_eq!(fg("9"), None);
        assert_eq!(fg("1000"), Some(240.0));
        assert_eq!(bg("0"), None, "后台不许停到 0");
        assert_eq!(bg("1"), Some(1.0));
        assert_eq!(bg("120"), Some(60.0));
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
