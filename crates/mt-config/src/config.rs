//! `AppConfig` 的 schema、迁移与磁盘读写。
//!
//! 与 Tauri 版的差别只有两处出入口:路径不再来自 `AppHandle`(见 [`crate::paths`]),
//! 写盘令牌不再是 Tauri managed state 而是 [`ConfigStore`] 自己的字段。
//! **存量字段的序列化形状不动**；新增字段必须带 serde 缺省 —— 存量
//! `config.json` 必须原样读得进来。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

/// SSH 连接(`config.json` 的 `sshConnections` 数组元素)。
///
/// 收尾-1 批去重:迁移期这里是 `mt_core::SshConnection` 的逐字段复刻(那时
/// mt-core 还在 `src-tauri/` 下,新工作区反向依赖旧目录树会把整套 Tauri 依赖
/// 拖进来)。mt-core 物理移入 `crates/` 后复刻已删,改为**再导出 mt-core 的定义**。
///
/// **方向为什么是 mt-config → mt-core,而不是反过来**:两个方向都不成环
/// (mt-core 依赖表只有 serde/serde_json/dirs,mt-config 此前不依赖 mt-core),
/// 但 mt-core 是依赖图的**叶子**,被 miniterm-hook / mt-ssh-mcp / mt-ssh-cli
/// 三个独立小二进制与 mt-ssh 直接链接。若反过来让 mt-core `pub use`
/// mt-config 的定义,zip / sha2 / anyhow 整棵树会被拖进 hook 小二进制,而且
/// mt-core 的 `config_reader`(sidecar 自持的 config.json 读取器)会与
/// mt-config 的 `paths` / `ConfigStore` 在同一依赖树里重影。取代价小的那个。
///
/// 「config 是 sshConnections 的持久化归属方」这条决议本身不变:
/// - 名字仍是 `mt_config::SshConnection`,其他 crate 的引用一行不用改;
/// - `config.json` 仍由本 crate 读写;
/// - 钉住磁盘格式的 serde 形状回归测试
///   (`tests::ssh_connection_uses_camel_case_and_skips_none`)原样保留,
///   现在钉的直接是那份共享定义,护栏比原来更硬。
pub use mt_core::SshConnection;

// 注意：variant 顺序不可调换！untagged 按声明顺序尝试匹配
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProjectTreeItem {
    ProjectId(String),
    Group(ProjectGroup),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectGroup {
    pub id: String,
    pub name: String,
    pub collapsed: bool,
    pub children: Vec<ProjectTreeItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OldProjectGroup {
    pub id: String,
    pub name: String,
    pub collapsed: bool,
    pub project_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub projects: Vec<ProjectConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_tree: Option<Vec<ProjectTreeItem>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_groups: Option<Vec<OldProjectGroup>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_ordering: Option<Vec<String>>,
    pub default_shell: String,
    pub available_shells: Vec<ShellConfig>,
    #[serde(default = "default_ui_font_size")]
    pub ui_font_size: f64,
    #[serde(default = "default_terminal_font_size")]
    pub terminal_font_size: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_font_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_font_family: Option<String>,
    #[serde(default)]
    pub terminal_ligatures: bool,
    /// 每个终端保留的回滚行数(scrollback)。
    ///
    /// 这个值原本是 WebView renderer 内存的大头:xterm 每行按 `Uint32Array(cols * 3)`
    /// 分配,即 cols × 12 字节,120 列约 1.5KB/行。原先硬编码 10 万行意味着
    /// 单个终端最高吃掉 150-250MB。默认降到 1 万行,需要更长历史的用户可自行调高。
    /// (GPUI 侧换 `alacritty_terminal` 的 grid 后单行开销另算,但语义与上限含义不变。)
    #[serde(default = "default_terminal_scrollback")]
    pub terminal_scrollback: u32,
    // ─── 以下五个字段已搬进 `layout.db`(见 `mt-layout`)───────────────────
    //
    // **只读不写**:保留反序列化是为了给存量 `config.json` 做一次性迁移,
    // 顺带让「装了新版又降级回旧版」的用户仍能开起来(布局停在迁移那一刻,
    // 而不是整个丢失)。序列化一律 skip —— 磁盘归属已经换人,再写回去就成了
    // 两个来源互相打架。观察一个版本后连字段一起删。
    //
    // 运行期这些字段仍是 `AppStore` 手上的**内存缓存**:启动时由 layout.db 的值
    // 覆盖进来,各处 getter 照旧读它,只有落盘那一步改道。
    #[serde(default, skip_serializing)]
    pub layout_sizes: Option<Vec<f64>>,
    #[serde(default, skip_serializing)]
    pub middle_column_sizes: Option<Vec<f64>>,
    #[serde(default = "default_theme")]
    pub theme: String,
    // ⚠️ 曾经这里还有个 `skin`（内置皮肤 none/blueprint/fluent2）。GPUI 侧从来
    // 没有对应的色表，一律按 `none` 渲染，设置里那一栏也已整段移除，字段随之删掉。
    // 存量库/存量 `config.json` 里残留的 `skin` 键会被静默忽略（本结构不开
    // `deny_unknown_fields`），下一次落盘时由 `db.rs` 的 stale key 清理顺手删掉。
    /// 界面语言。取值 `"zh"` / `"en"`，与 TS 侧存进 localStorage 的那个字符串
    /// 一模一样（`mt_i18n::Locale` 的序列化约定就是这两个小写码），迁移期两套
    /// 可互读。`None` = 用户从未选过，由 mt-app 首启按系统语言探测。
    ///
    /// **存 String 而不是枚举**:与紧邻的 `theme` 同一取舍 —— 这个字段
    /// 手改坏了（比如填了 `"fr"`）不该让整份 `config.json` 反序列化失败，那会
    /// 连带把项目列表一起丢掉。合法性交给使用点的 `Locale::from_code` 判，
    /// 认不出就当没设过。Tauri 版的 `AppConfig` 没有这个字段，但它同样不开
    /// `deny_unknown_fields`，多出来的键会被静默忽略（见 `locale_是纯增量字段`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(default = "default_terminal_follow_theme")]
    pub terminal_follow_theme: bool,
    #[serde(default = "default_ai_completion_popup")]
    pub ai_completion_popup: bool,
    #[serde(default = "default_ai_completion_taskbar_flash")]
    pub ai_completion_taskbar_flash: bool,
    #[serde(default = "default_true")]
    pub ai_completion_sound: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_completion_sound_path: Option<String>,
    /// AI 转入「待确认」时是否也走完成通知的三个通道（弹框 / 任务栏 / 提示音）。
    /// 旧配置没有该字段，`default_true` 让升级上来的用户默认拿到提醒
    #[serde(default = "default_true")]
    pub ai_attention_notify: bool,
    #[serde(default)]
    pub editors: Vec<EditorConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_editor: Option<String>,
    /// 旧字段，仅用于反序列化迁移，序列化时跳过
    #[serde(default, skip_serializing)]
    pub vscode_path: Option<String>,
    #[serde(default = "default_git_changes_view_mode")]
    pub git_changes_view_mode: String,
    #[serde(default = "default_true")]
    pub long_paste_to_file: bool,
    #[serde(default = "default_long_paste_line_threshold")]
    pub long_paste_line_threshold: u32,
    #[serde(default = "default_long_paste_char_threshold")]
    pub long_paste_char_threshold: u32,
    /// 远程项目粘贴落盘目录:剪贴板图片 / 长文本转存的临时文件经 SFTP 上传到这里，
    /// 粘进终端的是远端路径（本地路径远端 agent 读不到）。
    /// 相对路径 = 相对项目根（默认落项目内，agent 无需额外授权即可读）；
    /// 也可填远端绝对路径（`/tmp/mini-term`）或 `~/xxx`。含 `..` 的写法会被拒绝。
    #[serde(default = "default_remote_paste_dir")]
    pub remote_paste_dir: String,
    /// 文件管理器下载到本机时的目标目录。
    ///
    /// `None` = 跟随系统下载目录（系统 API 不可用时回落到 `$HOME/Downloads`）；
    /// `Some` = 用户在设置页显式选择的本地目录。增量字段必须允许旧配置缺省。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_dir: Option<String>,
    // NOTE: 曾有 projects_visible / sessions_visible / files_visible / git_visible
    // 四个面板显隐开关，界面上没有任何入口消费（已被 middle_column_visible 与右侧
    // 抽屉取代），随 UI 改版一并删除。旧 config.json 里残留的这些键会被 serde 忽略。
    /// 已搬进 `layout.db`,只读不写(理由见 [`AppConfig::layout_sizes`] 上方那段)。
    #[serde(default = "default_true", skip_serializing)]
    pub middle_column_visible: bool,
    /// 已搬进 `layout.db`,只读不写。
    #[serde(default, skip_serializing)]
    pub right_drawer_width: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_project_id: Option<String>,
    #[serde(default)]
    pub hook_enabled: bool,
    #[serde(default)]
    pub smart_copy_paste: bool,
    /// 拖选按住不动自动复制的静止时长(秒)。`None` = UI 层默认 1s。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_auto_copy_secs: Option<f64>,
    /// 状态栏(系统托盘 / 菜单栏)项目状态灯总开关。`None` = UI 层默认开启。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tray_status_enabled: Option<bool>,
    /// 托盘右键菜单最多显示的活跃项目数。`None` = UI 层默认 5。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tray_max_projects: Option<u32>,
    /// 左键点状态栏图标时是否顺带定位到「下一个该处理」的会话。
    /// `None` = UI 层默认开启;关掉则只唤起窗口，不改变当前视图。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tray_click_focus: Option<bool>,
    /// 终端区换场动画总开关（切 tab/切面板/最大化/拆分）。`None` = UI 层默认开启。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_animations: Option<bool>,
    /// 启动恢复布局后是否自动续接上次的 AI 会话（往 pane 写 resume 命令）。
    /// `None` = UI 层默认开启（保持旧行为）。关掉只是不写命令，会话身份仍随布局
    /// 持久化，重新打开开关后下次启动照样能续上。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_auto_resume: Option<bool>,
    #[serde(default)]
    pub ssh_connections: Vec<SshConnection>,
    /// 显式创建的 SSH 分组名（允许空分组存在）。连接上的 group 字段仍是归属的
    /// 单一来源，此列表只补充「还没有连接的分组」；空 Vec 时序列化跳过。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_groups: Vec<String>,
    /// 移动端中转配置(docs/adr/0001)。None = 未启用;序列化时省略保持文件干净。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mobile_relay: Option<MobileRelayConfig>,
    /// 激活的外置主题包 id（themes/ 下目录名）。None = 内置外观模式;
    /// 激活时 `theme` 保持不动，退出自定义主题可无损回落。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_theme_id: Option<String>,
    /// AI 历史面板的会话列表视图。None = 默认平铺（"flat" | "tree"）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_list_view: Option<String>,
    /// 会话分支自记账边（mini-term 自己发起的 fork 当场记下 child→parent）。
    /// 磁盘扫描（scan_session_lineage）是权威来源，这里只兜「会话文件尚未落盘
    /// 的窗口期」与无磁盘指针的场景；合并时按 child id 去重、磁盘优先。
    /// 缺字段会被保存路径的强类型反序列化静默丢弃，default 必须齐。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub session_lineage: Vec<SavedLineageEdge>,
    // === 用量面板偏好 ===
    //
    // 旧版存 localStorage 六个键（`UsageStatsModal.tsx:15-20`）；GPUI 壳里没有
    // localStorage，配置文件是唯一的持久层（与 `locale` 同一条理由）。
    //
    // **全部 Option/宽松类型**:手改坏值不许拖垮整份 config 连带丢掉项目列表。
    // 读取端一律过白名单/正则，认不出就回默认（**不写回、不报错**，与旧版
    // `loadPref` 同）。`skip_serializing_if` 保旧格式互读。
    /// `"all" | "claude" | "codex" | "grok"`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_scope: Option<String>,
    /// `"today" | "days7" | "days30" | "month" | "months3" | "months6" | "custom"`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_range: Option<String>,
    /// 单项目 scope 的**原始路径**;None = 整机。项目被移除时渲染期回落整机，
    /// 不在读盘时一次性判定（面板开着时项目也可能被删）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_project: Option<String>,
    /// 自动刷新间隔(秒);0 = 关。合法档位 0/5/10/30/60。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_auto_refresh: Option<u32>,
    /// custom range 起始日 `"YYYY-MM-DD"`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_custom_from: Option<String>,
    /// custom range 截止日 `"YYYY-MM-DD"`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_custom_to: Option<String>,
}

/// 自记账的会话分支边（与 `mt-ai` 侧 `LineageEdge` 同构，独立定义避免
/// config 序列化面依赖扫描模块的输出类型）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedLineageEdge {
    pub agent: String,
    pub session_id: String,
    pub parent_session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_point_uuid: Option<String>,
}

/// 移动端中转体系的持久化配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MobileRelayConfig {
    /// 中转服务器地址(如 wss://relay.example.com);空字符串 = 未配置、不建连。
    #[serde(default)]
    pub relay_url: String,
    /// 桌面端接入密钥:必须与中转的 `MT_RELAY_DESKTOP_KEY` 一致,握手时携带。
    /// 空字符串 = 未填,中转一律拒绝(fail-closed,见 ADR 0002)。
    #[serde(default)]
    pub desktop_key: String,
    /// AI 启动器列表:移动端能发起哪些 agent 由此决定。
    /// 命令与 shell 只存在于桌面端配置里,移动端只见 id 与展示名。
    /// 旧配置缺该字段时填充预置两条(Claude / Codex),开箱即用。
    #[serde(default = "default_launchers")]
    pub launchers: Vec<AiLauncher>,
}

impl Default for MobileRelayConfig {
    fn default() -> Self {
        Self {
            relay_url: String::new(),
            desktop_key: String::new(),
            launchers: default_launchers(),
        }
    }
}

/// 一条具名的"怎么起一个 AI 会话"。
///
/// 启动流程是:按 `shell` 建 pane(缺省用 `default_shell`)→ 把 `command` 连同回车
/// 写入 PTY。AI 会话身份靠输入检测建立,所以命令必须走"敲进 shell"这条路。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AiLauncher {
    pub id: String,
    /// 展示名(移动端弹层里看到的就是它)
    pub name: String,
    /// 引用 `available_shells` 里的条目名;None / 空 = 用 `default_shell`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    pub command: String,
}

/// 预置启动器:零配置直接可用。
fn default_launchers() -> Vec<AiLauncher> {
    vec![
        AiLauncher {
            id: "claude".into(),
            name: "Claude".into(),
            shell: None,
            command: "claude".into(),
        },
        AiLauncher {
            id: "codex".into(),
            name: "Codex".into(),
            shell: None,
            command: "codex".into(),
        },
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedPane {
    pub shell_name: String,
    /// 工作目录覆盖(worktree 终端):有值则替代项目根作为 PTY cwd
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 退出时该 pane 正在跑的 AI 会话(hook 上报的精确身份)。
    /// 重启恢复布局后据此写入 `claude --resume` / `codex resume` 续接会话。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_session: Option<SavedAiSession>,
}

/// SavedPane 里持久化的 AI 会话身份。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedAiSession {
    /// 来源 agent(claude-code / codex),缺省按 Claude 处理
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// 会话的启动目录。`claude --resume` 只认「启动目录」对应的会话桶,起于子
    /// 目录的会话在项目根恢复会报 No conversation found。缺这个字段时 serde 会
    /// 静默丢弃写进 savedLayout 的 cwd,hook 第一手上报的启动目录与
    /// 反查结果都存不下来,每次重启只能重查一遍。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum SavedSplitNode {
    Leaf {
        /// 旧格式（单个 pane），仅用于反序列化兼容，序列化时跳过
        #[serde(default, skip_serializing)]
        pane: Option<SavedPane>,
        /// 新格式（pane 数组），当前始终使用此字段
        #[serde(default)]
        panes: Vec<SavedPane>,
    },
    Split {
        direction: String,
        children: Vec<SavedSplitNode>,
        sizes: Vec<f64>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedTab {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
    pub split_layout: SavedSplitNode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedProjectLayout {
    pub tabs: Vec<SavedTab>,
    pub active_tab_index: usize,
}

/// 项目级环境变量。注入到该项目新建终端 PTY 的子进程,与 portable-pty 默认继承的
/// 父进程 env 合并(同名 key 覆盖)。已开终端不受影响。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectEnvVar {
    pub key: String,
    pub value: String,
    /// 取消勾选时 value 保留但不注入;允许用户临时禁用某变量而无需删行重输。
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectConfig {
    pub id: String,
    pub name: String,
    pub path: String,
    /// 需求描述,显示在项目名后的灰色小字。`None` = 不显示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 分屏树。**已搬进 `layout.db`,只读不写** —— 保留反序列化供一次性迁移
    /// (理由见 [`AppConfig::layout_sizes`] 上方那段)。运行期仍作为内存缓存,
    /// 由 `AppStore` 在启动时用 layout.db 的值填进来。
    #[serde(default, skip_serializing)]
    pub saved_layout: Option<SavedProjectLayout>,
    #[serde(default)]
    pub expanded_dirs: Vec<String>,
    /// 是否已为该项目启用 SSH 工具（字段名保留 MCP 以兼容存量配置）。
    #[serde(default)]
    pub ssh_mcp_enabled: bool,
    /// CLI/daemon 项目能力令牌。随机生成并写入项目 SKILL.md，用于不可伪造地
    /// 解析该项目的 SSH 连接范围；旧配置缺失时在下次保存「关联 SSH」时迁移。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_cli_token: Option<String>,
    /// 该项目的 agent 可访问的 SSH 连接 id 列表（「关联 SSH」设定的范围）。
    /// `None` = 未设置 → 默认全部连接可见。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_connection_ids: Option<Vec<String>>,
    /// 项目级环境变量列表,新建终端时注入。空 Vec 时序列化跳过保持文件干净。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_vars: Vec<ProjectEnvVar>,
    /// WSL 会话来源发行版名(「WSL 关联项目」的声明)。`None` = 未启用。
    /// WSL 根项目(UNC 路径)不落此配置,distro 从路径自动推导。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wsl_sessions_distro: Option<String>,
    /// SSH 远程项目(task 07-05):有值即远程项目,指向 `sshConnections` 里
    /// 一条连接的 id;此时 `path` 存**远程 POSIX 绝对路径**(如 `/home/u/proj`)。
    /// 引用为单一来源、不内嵌连接快照——连接被删除时项目进入「断链」错误态。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_connection_id: Option<String>,
    /// 子项目(worktree「设为项目」):有值 = 挂在该项目 id 下渲染,不在 projectTree 里
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_project_id: Option<String>,
    /// 项目类型徽标覆盖:`None` = 自动探测,"none" = 不显示,其余为技术栈 key。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellConfig {
    pub name: String,
    pub command: String,
    pub args: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorConfig {
    pub name: String,
    pub command: String,
}

fn default_ui_font_size() -> f64 {
    13.0
}
fn default_terminal_font_size() -> f64 {
    14.0
}
fn default_terminal_scrollback() -> u32 {
    10000
}
fn default_theme() -> String {
    "auto".into()
}
fn default_terminal_follow_theme() -> bool {
    true
}
fn default_ai_completion_popup() -> bool {
    true
}
fn default_ai_completion_taskbar_flash() -> bool {
    true
}
fn default_git_changes_view_mode() -> String {
    "list".into()
}
fn default_long_paste_line_threshold() -> u32 {
    10
}
fn default_long_paste_char_threshold() -> u32 {
    2000
}
/// 默认落项目内的隐藏目录:agent 对项目目录天然有读权限，不像 `/tmp` 那样
/// 会触发 Claude Code 的项目外路径确认。
pub fn default_remote_paste_dir() -> String {
    ".mini-term/pasted".into()
}
fn default_true() -> bool {
    true
}

static DOWNLOAD_DIR_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn resolve_download_dir_with(
    configured: Option<&str>,
    system_download_dir: Option<PathBuf>,
    home_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(path) = configured {
        let path = PathBuf::from(path);
        return if path.is_absolute() {
            Ok(path)
        } else {
            Err(anyhow!("下载目录必须是绝对路径：{}", path.display()))
        };
    }
    system_download_dir
        .or_else(|| home_dir.map(|home| home.join("Downloads")))
        .ok_or_else(|| anyhow!("无法确定本机下载目录：系统下载目录和用户主目录均不可用"))
}

impl AppConfig {
    /// 解析系统默认下载目录，不读取用户覆盖值。
    pub fn system_download_dir() -> Result<PathBuf> {
        resolve_download_dir_with(None, dirs::download_dir(), dirs::home_dir())
    }

    /// 返回当前配置实际生效的下载目录。
    ///
    /// 显式配置优先；未配置时动态解析系统目录，避免把某台机器的默认路径写进配置。
    pub fn resolved_download_dir(&self) -> Result<PathBuf> {
        resolve_download_dir_with(
            self.download_dir.as_deref(),
            dirs::download_dir(),
            dirs::home_dir(),
        )
    }

    /// 校验设置页选中的下载目录真实存在、是绝对目录且当前进程可创建并删除文件。
    ///
    /// 该方法会短暂创建一个空探针文件；调用方应放到后台线程执行。下载动作开始前仍应
    /// 再调用一次，以覆盖设置后目录被删除或权限变化的情况。
    pub fn validate_download_dir(path: &Path) -> Result<()> {
        if !path.is_absolute() {
            return Err(anyhow!("下载目录必须是绝对路径：{}", path.display()));
        }

        let metadata = fs::metadata(path)
            .map_err(|err| anyhow!("无法访问下载目录 {}：{err}", path.display()))?;
        if !metadata.is_dir() {
            return Err(anyhow!("下载路径不是文件夹：{}", path.display()));
        }

        let sequence = DOWNLOAD_DIR_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let probe = path.join(format!(
            ".mini-term-write-probe-{}-{timestamp}-{sequence}",
            std::process::id(),
        ));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)
            .map_err(|err| anyhow!("下载目录不可写 {}：{err}", path.display()))?;
        drop(file);
        fs::remove_file(&probe)
            .map_err(|err| anyhow!("下载目录无法清理写入探针 {}：{err}", probe.display()))?;
        Ok(())
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            projects: vec![],
            project_tree: None,
            project_groups: None,
            project_ordering: None,
            default_shell: default_shell_name(),
            available_shells: default_shells(),
            ui_font_size: default_ui_font_size(),
            terminal_font_size: default_terminal_font_size(),
            ui_font_family: None,
            terminal_font_family: None,
            terminal_ligatures: false,
            terminal_scrollback: default_terminal_scrollback(),
            layout_sizes: None,
            middle_column_sizes: None,
            theme: default_theme(),
            locale: None,
            terminal_follow_theme: default_terminal_follow_theme(),
            ai_completion_popup: default_ai_completion_popup(),
            ai_completion_taskbar_flash: default_ai_completion_taskbar_flash(),
            ai_completion_sound: true,
            ai_completion_sound_path: None,
            ai_attention_notify: true,
            editors: vec![],
            default_editor: None,
            vscode_path: None,
            git_changes_view_mode: default_git_changes_view_mode(),
            long_paste_to_file: true,
            long_paste_line_threshold: default_long_paste_line_threshold(),
            long_paste_char_threshold: default_long_paste_char_threshold(),
            remote_paste_dir: default_remote_paste_dir(),
            download_dir: None,
            middle_column_visible: true,
            right_drawer_width: None,
            last_active_project_id: None,
            hook_enabled: false,
            smart_copy_paste: false,
            selection_auto_copy_secs: None,
            tray_status_enabled: None,
            tray_max_projects: None,
            tray_click_focus: None,
            terminal_animations: None,
            ai_auto_resume: None,
            ssh_connections: vec![],
            ssh_groups: vec![],
            mobile_relay: None,
            custom_theme_id: None,
            session_list_view: None,
            session_lineage: vec![],
            usage_scope: None,
            usage_range: None,
            usage_project: None,
            usage_auto_refresh: None,
            usage_custom_from: None,
            usage_custom_to: None,
        }
    }
}

#[cfg(target_os = "windows")]
fn default_shell_name() -> String {
    "cmd".into()
}

#[cfg(target_os = "macos")]
fn default_shell_name() -> String {
    "zsh".into()
}

#[cfg(target_os = "linux")]
fn default_shell_name() -> String {
    "bash".into()
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn default_shell_name() -> String {
    "sh".into()
}

#[cfg(target_os = "windows")]
fn default_shells() -> Vec<ShellConfig> {
    vec![
        ShellConfig {
            name: "cmd".into(),
            command: "cmd".into(),
            args: None,
        },
        ShellConfig {
            name: "powershell".into(),
            command: "powershell".into(),
            args: None,
        },
        ShellConfig {
            name: "pwsh".into(),
            command: "pwsh".into(),
            args: None,
        },
    ]
}

#[cfg(target_os = "macos")]
fn default_shells() -> Vec<ShellConfig> {
    vec![
        ShellConfig {
            name: "zsh".into(),
            command: "/bin/zsh".into(),
            args: Some(vec!["--login".into()]),
        },
        ShellConfig {
            name: "bash".into(),
            command: "/bin/bash".into(),
            args: Some(vec!["--login".into()]),
        },
    ]
}

#[cfg(target_os = "linux")]
fn default_shells() -> Vec<ShellConfig> {
    vec![
        ShellConfig {
            name: "bash".into(),
            command: "/bin/bash".into(),
            args: None,
        },
        ShellConfig {
            name: "zsh".into(),
            command: "/usr/bin/zsh".into(),
            args: None,
        },
        ShellConfig {
            name: "sh".into(),
            command: "/bin/sh".into(),
            args: None,
        },
    ]
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
fn default_shells() -> Vec<ShellConfig> {
    vec![ShellConfig {
        name: "sh".into(),
        command: "/bin/sh".into(),
        args: None,
    }]
}

/// 把一份 [`SavedProjectLayout`] 就地归一化(旧格式 `pane` → `panes`)。
///
/// 布局搬进 `layout.db` 后,读出来的那一刻同样要过这一遍 —— 迁移是逐字节搬
/// JSON,旧形状会原样进库。归一化口径只有这一份,`mt-layout` 直接调它。
pub fn normalize_saved_layout(layout: &mut SavedProjectLayout) {
    for tab in layout.tabs.iter_mut() {
        normalize_split_node(&mut tab.split_layout);
    }
}

/// 将旧格式 `pane`（单个）迁移到新格式 `panes`（数组）
fn normalize_split_node(node: &mut SavedSplitNode) {
    match node {
        SavedSplitNode::Leaf { pane, panes } => {
            // take() 无论如何都要执行:旧字段读完即清,序列化时才不会又写回去
            if let Some(p) = pane.take()
                && panes.is_empty()
            {
                panes.push(p);
            }
        }
        SavedSplitNode::Split { children, .. } => {
            for child in children.iter_mut() {
                normalize_split_node(child);
            }
        }
    }
}

/// 逐代累积的 config 迁移。每次从磁盘读出来都要过一遍(含 `AppConfig::default()`,
/// 首启用户也得拿到预置的移动端启动器)。
pub fn migrate_config(mut config: AppConfig) -> AppConfig {
    // 迁移 vscodePath → editors
    if config.editors.is_empty()
        && let Some(path) = config.vscode_path.as_ref()
    {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            config.editors.push(EditorConfig {
                name: "VS Code".into(),
                command: trimmed.into(),
            });
            config.default_editor = Some("VS Code".into());
        }
    }
    config.vscode_path = None;

    // 移动端配置整块缺失(从未用过移动端)→ 补一份缺省,让「移动端」面板一打开
    // 就有预置启动器可用。只补整块缺失的情况:`launchers: []` 是用户删光的有意
    // 结果,不能被"好心"重新填上。
    if config.mobile_relay.is_none() {
        config.mobile_relay = Some(MobileRelayConfig::default());
    }

    // 迁移 SavedSplitNode: pane → panes
    for project in config.projects.iter_mut() {
        if let Some(layout) = project.saved_layout.as_mut() {
            normalize_saved_layout(layout);
        }
    }

    if config.project_tree.is_some() {
        config.project_groups = None;
        config.project_ordering = None;
        return config;
    }
    let groups = match config.project_groups.take() {
        Some(g) if !g.is_empty() => g,
        _ => return config,
    };
    let ordering = config.project_ordering.take().unwrap_or_default();
    let group_map: std::collections::HashMap<String, &OldProjectGroup> =
        groups.iter().map(|g| (g.id.clone(), g)).collect();

    let mut tree: Vec<ProjectTreeItem> = Vec::new();
    for item_id in &ordering {
        if let Some(old_group) = group_map.get(item_id) {
            tree.push(ProjectTreeItem::Group(ProjectGroup {
                id: old_group.id.clone(),
                name: old_group.name.clone(),
                collapsed: old_group.collapsed,
                children: old_group
                    .project_ids
                    .iter()
                    .map(|pid| ProjectTreeItem::ProjectId(pid.clone()))
                    .collect(),
            }));
        } else {
            tree.push(ProjectTreeItem::ProjectId(item_id.clone()));
        }
    }
    config.project_tree = Some(tree);
    config
}

/// 读取并解析配置文件；主文件损坏时尝试上一代备份 .bak 自愈。
/// `Ok(Some)` = 成功（可能来自备份）；`Ok(None)` = 主文件不存在（首次启动）；
/// `Err` = 主文件损坏且备份不可用。[`ConfigStore::load`] 与 [`ConfigStore::read`]
/// 共用，保证「备份自愈」对 UI 与后台启动路径(hook/relay)同时生效。
pub fn read_config_from(path: &Path) -> Result<Option<AppConfig>> {
    match fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str(&content) {
            Ok(parsed) => Ok(Some(migrate_config(parsed))),
            Err(parse_err) => {
                let bak = path.with_extension("json.bak");
                match fs::read_to_string(&bak)
                    .ok()
                    .and_then(|c| serde_json::from_str(&c).ok())
                {
                    Some(parsed) => {
                        eprintln!(
                            "[config] config.json 解析失败({}), 已用备份 {} 恢复",
                            parse_err,
                            bak.display()
                        );
                        Ok(Some(migrate_config(parsed)))
                    }
                    None => Err(anyhow!(
                        "配置文件损坏且备份不可用: {} ({})",
                        path.display(),
                        parse_err
                    )),
                }
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow!("配置文件读取失败: {} ({})", path.display(), e)),
    }
}

// NOTE: 这里曾有 `should_backup` ——「覆写 config.json 前是否留一代 .bak」的判据。
// 配置本体搬进 `config.db` 后,写 config.json 的只剩派生的 SSH 投影(丢了从库里
// 随时再生),备份改由 `ConfigDb::backup_to` 在每次 load 后做一代。存量用户那份
// 完整的旧配置另存为 `config.json.pre-sqlite`,不参与轮换。

/// 一次成功加载的产物:配置 + 本次写盘令牌。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub token: u64,
}

/// [`ConfigStore::save`] 的失败原因。
///
/// `StaleToken` 单列一支而不是塞进一条错误字符串:调用方要据此决定"重新 load
/// 再合并重试",而不是把失败当写盘故障弹给用户。
#[derive(Debug)]
pub enum SaveError {
    /// 令牌过期或从未发放 —— 期间有别处写过配置,当前这份是基于陈旧快照改的。
    StaleToken { provided: u64, current: u64 },
    /// 配置序列化失败(理论上不该发生)。
    Serialize(serde_json::Error),
    /// 写盘失败(盘满 / 权限 / 杀软锁文件)。
    Io(std::io::Error),
    /// 配置库写入失败(库损坏 / 盘满 / 被占用)。
    Db(anyhow::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleToken { provided, current } => write!(
                f,
                "config token stale; reload config before saving (provided={provided}, current={current})"
            ),
            Self::Serialize(e) => write!(f, "配置序列化失败: {e}"),
            Self::Io(e) => write!(f, "配置写盘失败: {e}"),
            Self::Db(e) => write!(f, "配置库写入失败: {e:#}"),
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StaleToken { .. } => None,
            Self::Serialize(e) => Some(e),
            Self::Io(e) => Some(e),
            Self::Db(e) => Some(e.as_ref()),
        }
    }
}

/// 给 sidecar 读的 `config.json` 投影:只有 `sshConnections` 与 `projects[]` 的
/// 四个 SSH 字段。
///
/// **形状必须与 `mt_core::config_reader::ConfigSshView` 对得上** —— 那边是三个
/// sidecar 二进制自持的解析器(它们不依赖本 crate,每次请求重读这个文件做能力
/// 令牌鉴权)。字段名走 camelCase,`None` 直接省略(那边的字段都有 `default`)。
///
/// 项目**一个都不筛**:`scope_connections` 把「项目未找到」与「项目没设
/// sshConnectionIds」都判成"全部连接可见",漏写一个设过范围的项目就等于
/// 悄悄放宽了它的可见范围。
fn ssh_projection(config: &AppConfig) -> serde_json::Value {
    use serde_json::{Map, Value};
    let projects: Vec<Value> = config
        .projects
        .iter()
        .map(|p| {
            let mut m = Map::new();
            m.insert("id".into(), Value::String(p.id.clone()));
            m.insert("sshMcpEnabled".into(), Value::Bool(p.ssh_mcp_enabled));
            if let Some(token) = &p.ssh_cli_token {
                m.insert("sshCliToken".into(), Value::String(token.clone()));
            }
            if let Some(ids) = &p.ssh_connection_ids {
                m.insert(
                    "sshConnectionIds".into(),
                    Value::Array(ids.iter().cloned().map(Value::String).collect()),
                );
            }
            Value::Object(m)
        })
        .collect();

    let mut root = Map::new();
    root.insert(
        "sshConnections".into(),
        serde_json::to_value(&config.ssh_connections).unwrap_or(Value::Array(vec![])),
    );
    root.insert("projects".into(), Value::Array(projects));
    Value::Object(root)
}

/// 配置的读写口,同时持有**写盘令牌**。
///
/// # 磁盘上有两样东西
///
/// | 文件 | 归属 | 谁读 |
/// |---|---|---|
/// | `config.db` | **配置本体**(见 [`crate::db`]) | 只有主程序 |
/// | `config.json` | [`ssh_projection`] 的投影,派生物 | 三个 sidecar 二进制 |
///
/// `config.json` 曾经是配置的家,现在瘦身成投影 —— 那条 sidecar 链路必须原地
/// 不动的理由见 [`crate::db`] 的模块注释。投影**内容没变就不写**,所以改个字号
/// 不会碰它。
///
/// 存量用户的完整 `config.json` 在首次迁移时被另存为 `config.json.pre-sqlite`,
/// 不删不改 —— 那是回退到旧版本的唯一凭据。
///
/// 令牌是一个乐观并发计数:[`load`](Self::load) 每成功一次就轮换,
/// [`save`](Self::save) 必须携带当前令牌才允许写盘。不变量:**写盘的每一份配置,
/// 必然派生自当次成功的 load** ——
/// - 界面尚未初始化完(冷启动时组件的防抖保存、还没填过内容的空状态):
///   没有令牌或握着上一轮的过期令牌,保存被拒;
/// - 磁盘配置损坏导致加载失败:不发令牌,空默认配置永远拿不到写盘资格。
///
/// 0 = 从未发放,恒拒绝。原实现里这个计数是 Tauri 的 managed state
/// (`ConfigToken(AtomicU64)`),GPUI 下改由本结构持有,语义逐字不变;
/// 应用侧把它放进全局状态、各处共享同一个实例即可。
pub struct ConfigStore {
    /// `config.json`(投影)的路径。库路径由它的父目录推出来 ——
    /// 保持这个字段是为了 [`Self::at`] 的签名不变(dev 隔离与测试都传文件路径)。
    path: PathBuf,
    token: AtomicU64,
    /// 首次用到时才开库。开失败**不缓存**,下次调用重试 —— 盘暂时忙/被杀软
    /// 锁住这类瞬时故障不该让整个进程此后永远存不下配置。
    db: Mutex<Option<Arc<crate::db::ConfigDb>>>,
}

impl ConfigStore {
    /// 指向 `{app_data_dir}`,并顺手跑一次 identifier 迁移
    /// ——迁移必须早于任何一次读取,放在这里就无法忘记。
    pub fn open() -> Result<Self> {
        crate::paths::migrate_legacy_app_data();
        Ok(Self::at(crate::paths::config_path()?))
    }

    /// 指向任意 `config.json` 路径(测试与 dev 隔离目录用);
    /// `config.db` 落在它的同级目录。
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            token: AtomicU64::new(0),
            db: Mutex::new(None),
        }
    }

    /// `config.json`(给 sidecar 读的投影)的路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 数据目录 —— `config.db` 与投影同级。
    fn dir(&self) -> &Path {
        self.path.parent().unwrap_or(Path::new("."))
    }

    /// 完整旧配置的存档路径。迁移时另存一次,此后不删不改。
    fn legacy_archive_path(&self) -> PathBuf {
        self.path.with_extension("json.pre-sqlite")
    }

    fn db_backup_path(&self) -> PathBuf {
        self.dir().join("config.db.bak")
    }

    /// 取(必要时打开)配置库。
    fn db(&self) -> Result<Arc<crate::db::ConfigDb>> {
        let mut slot = self.db.lock().map_err(|_| anyhow!("配置库句柄锁中毒"))?;
        if let Some(db) = slot.as_ref() {
            return Ok(db.clone());
        }
        let db = Arc::new(crate::db::ConfigDb::open_at(self.dir())?);
        *slot = Some(db.clone());
        Ok(db)
    }

    /// 当前有效令牌。0 = 还没有过一次成功的 [`load`](Self::load)。
    pub fn current_token(&self) -> u64 {
        self.token.load(Ordering::Acquire)
    }

    /// 严格加载:库为空 = 首次启动或存量用户首次升级,前者拿默认配置、后者从
    /// `config.json` 一次性迁入;库损坏且备份不可用才返回错误——绝不把默认空配置
    /// 伪装成加载成功(那会让调用方拿着空配置开始运行,下一次保存就把库覆盖了)。
    ///
    /// 加载成功才轮换发放令牌;上一轮的令牌随之作废。
    pub fn load(&self) -> Result<LoadedConfig> {
        let db = self.db()?;
        let config = match db.load()? {
            Some(config) => migrate_config(config),
            None => self.import_from_json(&db)?,
        };
        // 每启动留一代库备份(配置不可再生,这是它与 layout.db 的关键差别)。
        // 失败只记日志:备份不该拦住启动。
        if let Err(err) = db.backup_to(&self.db_backup_path()) {
            eprintln!("[config] 配置库备份失败(不影响本次运行): {err:#}");
        }
        // 投影与库对齐 —— sidecar 读的是它。内容没变时是 no-op。
        self.write_ssh_projection(&config);

        let token = self.token.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        eprintln!("[config] load ok, token={token}");
        Ok(LoadedConfig { config, token })
    }

    /// 库是空的 → 从 `config.json` 灌一次(存量用户),或落一份默认配置(全新安装)。
    ///
    /// 灌之前先把**完整的**旧 config.json 另存为 `config.json.pre-sqlite`:
    /// 紧接着投影就会把 config.json 覆盖成只剩 SSH 的小文件,那份存档是回退到
    /// 旧版本的唯一凭据。
    fn import_from_json(&self, db: &crate::db::ConfigDb) -> Result<AppConfig> {
        let legacy = read_config_from(&self.path)?;
        let config = migrate_config(legacy.clone().unwrap_or_default());
        if legacy.is_some() {
            let archive = self.legacy_archive_path();
            if !archive.exists()
                && let Err(err) = fs::copy(&self.path, &archive)
            {
                eprintln!("[config] 旧 config.json 存档失败: {err}");
            }
            eprintln!(
                "[config] 已把 config.json 迁入 {}(项目 {} / SSH 连接 {},原文件存档于 {})",
                db.path().display(),
                config.projects.len(),
                config.ssh_connections.len(),
                self.legacy_archive_path().display()
            );
        }
        db.save(&config)?;
        Ok(config)
    }

    /// 容错加载:任何读取/解析失败都回退默认,且**不轮换令牌**。
    ///
    /// 供后台路径(hook / relay / 新建 PTY 取项目 env)只读取个别字段用——
    /// 它们不写盘,吞错无害;后台必须能启动,只在库与 config.json 均不可用时
    /// 才按默认运行。
    pub fn read(&self) -> AppConfig {
        match self.db().and_then(|db| db.load()) {
            Ok(Some(config)) => migrate_config(config),
            // 库还没迁移过(主窗口尚未 load 过一次)→ 退回读 config.json
            Ok(None) => match read_config_from(&self.path) {
                Ok(Some(config)) => config,
                _ => migrate_config(AppConfig::default()),
            },
            Err(e) => {
                eprintln!("[config] {e:#}; 后台本次按默认配置启动");
                migrate_config(AppConfig::default())
            }
        }
    }

    /// 带令牌写盘。令牌不匹配一律拒绝,调用方必须先 [`load`](Self::load) 再重试。
    pub fn save(&self, token: u64, config: &AppConfig) -> Result<(), SaveError> {
        let current = self.current_token();
        if token == 0 || token != current {
            eprintln!(
                "[config] REJECT save: token {} != current {} (projects={})",
                token,
                current,
                config.projects.len()
            );
            return Err(SaveError::StaleToken {
                provided: token,
                current,
            });
        }
        let db = self.db().map_err(SaveError::Db)?;
        db.save(config).map_err(SaveError::Db)?;
        self.write_ssh_projection(config);
        Ok(())
    }

    /// 把 SSH 投影写进 `config.json`。**内容没变就不写** ——
    /// 改个字号、切个主题都会走到这里,不该每次都碰这个文件
    /// (sidecar 每次请求都在读它)。
    ///
    /// 不留 `.bak`:投影是派生物,从库里随时能再生。存量用户那份完整备份叫
    /// `config.json.pre-sqlite`,由 [`import_from_json`](Self::import_from_json) 存下。
    fn write_ssh_projection(&self, config: &AppConfig) {
        let json = match serde_json::to_string_pretty(&ssh_projection(config)) {
            Ok(json) => json,
            Err(err) => {
                eprintln!("[config] SSH 投影序列化失败: {err}");
                return;
            }
        };
        if fs::read_to_string(&self.path).ok().as_deref() == Some(json.as_str()) {
            return;
        }
        if let Err(err) = atomic_write(&self.path, json.as_bytes()) {
            eprintln!("[config] SSH 投影写盘失败(sidecar 会读到上一版): {err}");
        }
    }
}

/// 同目录临时文件 + rename 的原子写。
///
/// 收尾-1 批把工作区里的三份逐字副本(本模块、`mt_project::fs`、`mt_ai::util`)
/// 合并进叶子 crate `mt-core`。原先「不能为一个 20 行工具函数把 mt-config 挂到
/// mt-project 上(依赖方向会倒过来)」的顾虑消失了:mt-core 依赖表只有
/// serde/serde_json/dirs,谁依赖它都不会把方向弄反。
/// 实现见 `mt_core::atomic_write`,行为与本文件原副本一字不差。
use mt_core::atomic_write;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_shells() {
        let config = AppConfig::default();
        assert!(!config.available_shells.is_empty());
        assert!(!config.default_shell.is_empty());
    }

    #[test]
    fn config_round_trip() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.available_shells.len(), config.available_shells.len());
    }

    #[test]
    fn download_dir_is_backward_compatible_and_round_trips() {
        let legacy = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": []
        }"#;
        let mut config: AppConfig = serde_json::from_str(legacy).unwrap();
        assert!(config.download_dir.is_none());
        assert!(
            !serde_json::to_string(&config)
                .unwrap()
                .contains("downloadDir"),
            "跟随系统默认时不应污染持久化配置"
        );

        config.download_dir = Some("/chosen/downloads".into());
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains(r#""downloadDir":"/chosen/downloads""#));
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.download_dir.as_deref(), Some("/chosen/downloads"));
    }

    #[test]
    fn download_dir_resolution_prefers_override_then_system_then_home() {
        let configured = std::env::temp_dir().join("chosen");
        let configured_text = configured.to_string_lossy().into_owned();
        let override_path = resolve_download_dir_with(
            Some(&configured_text),
            Some(PathBuf::from("system")),
            Some(PathBuf::from("home")),
        )
        .unwrap();
        assert_eq!(override_path, configured);

        let system = resolve_download_dir_with(
            None,
            Some(PathBuf::from("system")),
            Some(PathBuf::from("home")),
        )
        .unwrap();
        assert_eq!(system, PathBuf::from("system"));

        let fallback = resolve_download_dir_with(None, None, Some(PathBuf::from("home"))).unwrap();
        assert_eq!(fallback, PathBuf::from("home").join("Downloads"));
        assert!(resolve_download_dir_with(None, None, None).is_err());

        let config = AppConfig {
            download_dir: Some("relative".into()),
            ..AppConfig::default()
        };
        assert!(config.resolved_download_dir().is_err());
    }

    #[test]
    fn download_dir_validation_rejects_invalid_targets_and_cleans_probe() {
        let root = unique_test_root("download-dir-validation");
        AppConfig::validate_download_dir(&root).unwrap();
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);

        let file = root.join("not-a-directory");
        fs::write(&file, b"x").unwrap();
        assert!(AppConfig::validate_download_dir(&file).is_err());
        assert!(AppConfig::validate_download_dir(&root.join("missing")).is_err());
        assert!(AppConfig::validate_download_dir(Path::new("relative")).is_err());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn font_family_round_trip() {
        let json = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "uiFontFamily": "Arial, sans-serif",
            "terminalFontFamily": "'JetBrainsMono Nerd Font', monospace"
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ui_font_family.as_deref(), Some("Arial, sans-serif"));
        assert_eq!(
            config.terminal_font_family.as_deref(),
            Some("'JetBrainsMono Nerd Font', monospace")
        );
    }

    #[test]
    fn font_family_absent_is_none() {
        let json = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert!(config.ui_font_family.is_none());
        assert!(config.terminal_font_family.is_none());
    }

    /// `locale` 是纯增量字段:旧 config.json 没有它照样读得进来,没设过时不写盘
    /// (Tauri 版读到多余键会静默忽略,但少写一个键更省心),设过之后原样往返。
    #[test]
    fn locale_是纯增量字段() {
        let legacy = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(legacy).unwrap();
        assert!(config.locale.is_none(), "旧配置没有该字段不许炸");
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("locale"),
            "没选过语言就不该往磁盘上写这个键: {json}"
        );

        let with_locale = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "locale": "en"
        }"#;
        let config: AppConfig = serde_json::from_str(with_locale).unwrap();
        assert_eq!(config.locale.as_deref(), Some("en"));
        let reparsed: AppConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(reparsed.locale.as_deref(), Some("en"), "取值原样往返");
    }

    /// 取值认不出来时**不许**让整份配置反序列化失败 —— 那会连带丢掉项目列表。
    /// 合法性由使用点(`mt_i18n::Locale::from_code`)判,这里只保证读得进来。
    #[test]
    fn locale_取值非法不拖垮整份配置() {
        let json = r#"{
            "projects": [{"id": "1", "name": "test", "path": "/tmp"}],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "locale": "fr"
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.projects.len(), 1, "项目列表不受连累");
        assert_eq!(config.locale.as_deref(), Some("fr"));
    }

    /// 用量面板六个偏好键是纯增量字段:旧 config.json 读得进来、没设过不写盘、
    /// 设过之后原样往返(与 `locale_是纯增量字段` 同一条口径)。
    #[test]
    fn 用量偏好是纯增量字段() {
        let legacy = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(legacy).unwrap();
        assert!(config.usage_scope.is_none());
        assert!(config.usage_range.is_none());
        assert!(config.usage_project.is_none());
        assert!(config.usage_auto_refresh.is_none());
        assert!(config.usage_custom_from.is_none());
        assert!(config.usage_custom_to.is_none());
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            !json.contains("usage"),
            "没设过就不该往磁盘上写这些键: {json}"
        );

        let with_prefs = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "usageScope": "codex",
            "usageRange": "custom",
            "usageProject": "D:\\Git\\x",
            "usageAutoRefresh": 30,
            "usageCustomFrom": "2026-01-01",
            "usageCustomTo": "2026-02-01"
        }"#;
        let config: AppConfig = serde_json::from_str(with_prefs).unwrap();
        let reparsed: AppConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(reparsed.usage_scope.as_deref(), Some("codex"));
        assert_eq!(reparsed.usage_range.as_deref(), Some("custom"));
        assert_eq!(reparsed.usage_project.as_deref(), Some("D:\\Git\\x"));
        assert_eq!(reparsed.usage_auto_refresh, Some(30));
        assert_eq!(reparsed.usage_custom_from.as_deref(), Some("2026-01-01"));
        assert_eq!(reparsed.usage_custom_to.as_deref(), Some("2026-02-01"));
    }

    /// 用量偏好被手改成坏值时**不许**拖垮整份配置 —— 合法性交给使用点的白名单。
    #[test]
    fn 用量偏好取值非法不拖垮整份配置() {
        let json = r#"{
            "projects": [{"id": "1", "name": "test", "path": "/tmp"}],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "usageScope": "gemini",
            "usageRange": "all",
            "usageAutoRefresh": 7,
            "usageCustomFrom": "昨天"
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.projects.len(), 1, "项目列表不受连累");
        assert_eq!(config.usage_scope.as_deref(), Some("gemini"));
        assert_eq!(config.usage_range.as_deref(), Some("all"));
        assert_eq!(config.usage_auto_refresh, Some(7));
    }

    #[test]
    fn terminal_ligatures_round_trip() {
        let json = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "terminalLigatures": true
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert!(config.terminal_ligatures);

        let serialized = serde_json::to_string(&config).unwrap();
        let reparsed: AppConfig = serde_json::from_str(&serialized).unwrap();
        assert!(reparsed.terminal_ligatures);
    }

    #[test]
    fn terminal_ligatures_absent_defaults_false() {
        let json = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert!(!config.terminal_ligatures);
    }

    #[test]
    fn old_config_without_layout_deserializes() {
        let json = r#"{
            "projects": [{"id": "1", "name": "test", "path": "/tmp"}],
            "defaultShell": "cmd",
            "availableShells": [{"name": "cmd", "command": "cmd"}],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.projects.len(), 1);
        assert!(config.projects[0].saved_layout.is_none());
    }

    #[test]
    fn old_config_without_groups_deserializes() {
        let json = r#"{
            "projects": [{"id": "1", "name": "test", "path": "/tmp"}],
            "defaultShell": "cmd",
            "availableShells": [{"name": "cmd", "command": "cmd"}],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert!(config.project_tree.is_none());
        assert!(config.project_groups.is_none());
        assert!(config.project_ordering.is_none());
    }

    /// **布局字段只读不写**:磁盘归属已经换给 `layout.db`(见 `mt-layout`),
    /// config.json 再写这些键就成了两个来源互相打架 —— 用户拖完分隔条、
    /// 布局库写了新值,而后任意一次配置保存又会把旧值原样刷回去。
    ///
    /// 这一条钉的是决议本身,删字段那一版把整个测试一起删掉即可。
    #[test]
    fn 布局字段不再序列化() {
        let config = AppConfig {
            layout_sizes: Some(vec![20.0, 60.0, 20.0]),
            middle_column_sizes: Some(vec![50.0, 50.0]),
            middle_column_visible: false,
            right_drawer_width: Some(400.0),
            projects: vec![ProjectConfig {
                id: "p1".into(),
                name: "proj".into(),
                path: "/tmp".into(),
                description: None,
                saved_layout: Some(SavedProjectLayout {
                    tabs: vec![SavedTab {
                        custom_title: None,
                        split_layout: SavedSplitNode::Leaf {
                            pane: None,
                            panes: vec![SavedPane {
                                shell_name: "cmd".into(),
                                cwd: None,
                                ai_session: None,
                            }],
                        },
                    }],
                    active_tab_index: 0,
                }),
                expanded_dirs: vec![],
                ssh_mcp_enabled: false,
                ssh_cli_token: None,
                ssh_connection_ids: None,
                env_vars: vec![],
                wsl_sessions_distro: None,
                ssh_connection_id: None,
                parent_project_id: None,
                kind_override: None,
            }],
            ..Default::default()
        };

        let json = serde_json::to_string(&config).unwrap();
        for key in [
            "savedLayout",
            "layoutSizes",
            "middleColumnSizes",
            "middleColumnVisible",
            "rightDrawerWidth",
        ] {
            assert!(!json.contains(key), "{key} 不该再写进 config.json: {json}");
        }
    }

    /// 反过来:存量 config.json 里的这些键仍要**读得进来** —— 那是一次性迁移
    /// 进 `layout.db` 的唯一入口,读不出来存量用户的布局就直接蒸发了。
    #[test]
    fn 存量布局字段仍读得进来() {
        let json = r#"{
            "projects": [{
                "id": "1", "name": "test", "path": "/tmp",
                "savedLayout": {
                    "tabs": [{"splitLayout": {"type": "leaf", "panes": [{"shellName": "cmd"}]}}],
                    "activeTabIndex": 0
                }
            }],
            "defaultShell": "cmd",
            "availableShells": [{"name": "cmd", "command": "cmd"}],
            "layoutSizes": [20, 60, 20],
            "middleColumnSizes": [40, 60],
            "middleColumnVisible": false,
            "rightDrawerWidth": 400
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.layout_sizes, Some(vec![20.0, 60.0, 20.0]));
        assert_eq!(config.middle_column_sizes, Some(vec![40.0, 60.0]));
        assert!(!config.middle_column_visible);
        assert_eq!(config.right_drawer_width, Some(400.0));
        let layout = config.projects[0].saved_layout.as_ref().unwrap();
        assert_eq!(layout.tabs.len(), 1);
    }

    #[test]
    fn layout_round_trip() {
        let layout = SavedProjectLayout {
            tabs: vec![SavedTab {
                custom_title: Some("test".into()),
                split_layout: SavedSplitNode::Split {
                    direction: "horizontal".into(),
                    children: vec![
                        SavedSplitNode::Leaf {
                            pane: None,
                            panes: vec![SavedPane {
                                shell_name: "cmd".into(),
                                cwd: None,
                                ai_session: None,
                            }],
                        },
                        SavedSplitNode::Leaf {
                            pane: None,
                            panes: vec![SavedPane {
                                shell_name: "powershell".into(),
                                cwd: None,
                                ai_session: None,
                            }],
                        },
                    ],
                    sizes: vec![50.0, 50.0],
                },
            }],
            active_tab_index: 0,
        };
        let json = serde_json::to_string(&layout).unwrap();
        let parsed: SavedProjectLayout = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.tabs.len(), 1);
        assert_eq!(parsed.active_tab_index, 0);
    }

    #[test]
    fn migrate_old_groups_to_tree() {
        let json = r#"{
            "projects": [
                {"id": "p1", "name": "proj1", "path": "/tmp/1"},
                {"id": "p2", "name": "proj2", "path": "/tmp/2"}
            ],
            "projectGroups": [{"id": "g1", "name": "Group1", "collapsed": false, "projectIds": ["p1"]}],
            "projectOrdering": ["g1", "p2"],
            "defaultShell": "cmd",
            "availableShells": [{"name": "cmd", "command": "cmd"}],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        let config = migrate_config(config);
        assert!(config.project_tree.is_some());
        assert!(config.project_groups.is_none());
        assert!(config.project_ordering.is_none());
        let tree = config.project_tree.unwrap();
        assert_eq!(tree.len(), 2);
    }

    fn unique_test_root(label: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("mini-term-test-{label}-{ts}"));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn read_config_from_recovers_from_backup_when_main_corrupted() {
        let root = unique_test_root("read-bak-recover");
        let path = root.join("config.json");
        fs::write(&path, "{ corrupted").unwrap();
        let valid = serde_json::to_string(&AppConfig {
            default_shell: "bak-shell".into(),
            ..AppConfig::default()
        })
        .unwrap();
        fs::write(root.join("config.json.bak"), &valid).unwrap();

        let got = read_config_from(&path).unwrap().unwrap();
        assert_eq!(got.default_shell, "bak-shell");
    }

    #[test]
    fn read_config_from_errors_when_main_and_backup_both_unusable() {
        let root = unique_test_root("read-bak-none");
        let path = root.join("config.json");
        fs::write(&path, "{ corrupted").unwrap();
        assert!(read_config_from(&path).is_err());
    }

    #[test]
    fn read_config_from_none_when_missing() {
        let root = unique_test_root("read-missing");
        assert!(
            read_config_from(&root.join("config.json"))
                .unwrap()
                .is_none()
        );
    }

    // NOTE: 这里曾有 `corrupted_main_never_backed_up`(钉 config.json 的 .bak 轮换
    // 判据)。配置搬进 config.db 后那条路径不存在了,备份语义改由
    // `db::tests::备份可用于恢复` / `无备份时损坏必须报错` 钉住。

    #[test]
    fn env_vars_round_trip() {
        let json = r#"{
            "projects": [{
                "id": "p1",
                "name": "proj1",
                "path": "/tmp/1",
                "envVars": [
                    {"key": "FOO", "value": "bar", "enabled": true},
                    {"key": "API_KEY", "value": "sk-xxx", "enabled": false},
                    {"key": "EMPTY", "value": ""}
                ]
            }],
            "defaultShell": "cmd",
            "availableShells": [{"name": "cmd", "command": "cmd"}],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        let env_vars = &config.projects[0].env_vars;
        assert_eq!(env_vars.len(), 3);
        assert_eq!(env_vars[0].key, "FOO");
        assert_eq!(env_vars[0].value, "bar");
        assert!(env_vars[0].enabled);
        assert!(!env_vars[1].enabled);
        // enabled 字段缺省时默认 true
        assert_eq!(env_vars[2].key, "EMPTY");
        assert_eq!(env_vars[2].value, "");
        assert!(env_vars[2].enabled);

        // round-trip:再序列化再反序列化,字段顺序与值保持
        let serialized = serde_json::to_string(&config).unwrap();
        let reparsed: AppConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(reparsed.projects[0].env_vars.len(), 3);
        assert_eq!(reparsed.projects[0].env_vars[1].value, "sk-xxx");
    }

    #[test]
    fn env_vars_absent_is_empty_and_not_serialized() {
        // 旧 config.json 无 envVars 字段 → 默认空 Vec
        let json = r#"{
            "projects": [{"id": "p1", "name": "proj1", "path": "/tmp/1"}],
            "defaultShell": "cmd",
            "availableShells": [{"name": "cmd", "command": "cmd"}],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert!(config.projects[0].env_vars.is_empty());

        // 空 Vec 不写入 JSON,保持配置文件干净
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(
            !serialized.contains("envVars"),
            "空 envVars 不应序列化进 JSON: {serialized}"
        );
    }

    #[test]
    fn ssh_connection_id_round_trip_and_absent_default() {
        // 远程项目:sshConnectionId 有值,path 为远程 POSIX 绝对路径
        let json = r#"{
            "projects": [
                {"id": "p1", "name": "remote", "path": "/home/u/proj", "sshConnectionId": "conn-1"},
                {"id": "p2", "name": "local", "path": "D:\\Git\\x"}
            ],
            "defaultShell": "cmd",
            "availableShells": [{"name": "cmd", "command": "cmd"}],
            "uiFontSize": 13,
            "terminalFontSize": 14
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            config.projects[0].ssh_connection_id.as_deref(),
            Some("conn-1")
        );
        assert_eq!(config.projects[0].path, "/home/u/proj");
        // 旧配置无该字段 → None(向后兼容)
        assert!(config.projects[1].ssh_connection_id.is_none());

        // round-trip:camelCase 字段名保留;None 不写入 JSON
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(serialized.contains("\"sshConnectionId\":\"conn-1\""));
        assert_eq!(
            serialized.matches("sshConnectionId").count(),
            1,
            "本地项目不应序列化 sshConnectionId: {serialized}"
        );
        let reparsed: AppConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.projects[0].ssh_connection_id.as_deref(),
            Some("conn-1")
        );
    }

    #[test]
    fn ssh_groups_round_trip_and_absent_default() {
        // 显式分组列表:round-trip 保留顺序
        let json = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "sshGroups": ["内网", "客户A"]
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.ssh_groups, vec!["内网", "客户A"]);
        let serialized = serde_json::to_string(&config).unwrap();
        let reparsed: AppConfig = serde_json::from_str(&serialized).unwrap();
        assert_eq!(reparsed.ssh_groups, vec!["内网", "客户A"]);

        // 旧配置无该字段 → 空 Vec,且空时不序列化
        let old: AppConfig = serde_json::from_str(
            r#"{"projects":[],"defaultShell":"cmd","availableShells":[],"uiFontSize":13,"terminalFontSize":14}"#,
        )
        .unwrap();
        assert!(old.ssh_groups.is_empty());
        let serialized_old = serde_json::to_string(&old).unwrap();
        assert!(
            !serialized_old.contains("sshGroups"),
            "空 sshGroups 不应序列化进 JSON: {serialized_old}"
        );
    }

    #[test]
    fn ssh_connection_uses_camel_case_and_skips_none() {
        // SshConnection 是从 mt-core 复刻过来的,序列化面必须逐字段一致,
        // 否则 config.json 在新旧两套之间往返一次就会掉字段
        let conn = SshConnection {
            id: "1".into(),
            name: "prod".into(),
            host: "10.0.0.5".into(),
            port: 2222,
            user: "root".into(),
            password: None,
            identity_file: Some("/k".into()),
            group: Some("内网".into()),
        };
        let json = serde_json::to_string(&conn).unwrap();
        assert!(json.contains(r#""identityFile":"/k""#), "{json}");
        assert!(!json.contains("password"), "None 不应序列化: {json}");
        // 老配置里残留的 proxyJump 之类未知字段必须被静默忽略
        let parsed: SshConnection = serde_json::from_str(
            r#"{"id":"1","name":"n","host":"h","port":22,"user":"u","proxyJump":"user@bastion"}"#,
        )
        .unwrap();
        assert_eq!(parsed.port, 22);
        assert!(parsed.identity_file.is_none());
    }

    #[test]
    fn mobile_relay_round_trip_and_absent_default() {
        // 有值:camelCase 字段名往返保留
        let json = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "mobileRelay": {"relayUrl": "wss://relay.example.com", "desktopKey": "s3cret"}
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        let relay = config.mobile_relay.as_ref().unwrap();
        assert_eq!(relay.relay_url, "wss://relay.example.com");
        assert_eq!(relay.desktop_key, "s3cret");
        let serialized = serde_json::to_string(&config).unwrap();
        assert!(
            serialized.contains(r#""relayUrl":"wss://relay.example.com""#)
                && serialized.contains(r#""desktopKey":"s3cret""#),
            "{serialized}"
        );
        let reparsed: AppConfig = serde_json::from_str(&serialized).unwrap();
        let relay = reparsed.mobile_relay.unwrap();
        assert_eq!(relay.relay_url, "wss://relay.example.com");
        assert_eq!(relay.desktop_key, "s3cret");

        // 旧配置无该字段 → serde 层为 None,且 None 不序列化
        let old: AppConfig = serde_json::from_str(
            r#"{"projects":[],"defaultShell":"cmd","availableShells":[],"uiFontSize":13,"terminalFontSize":14}"#,
        )
        .unwrap();
        assert!(old.mobile_relay.is_none());
        let serialized_old = serde_json::to_string(&old).unwrap();
        assert!(
            !serialized_old.contains("mobileRelay"),
            "serde 层未配置时不应序列化 mobileRelay: {serialized_old}"
        );
    }

    #[test]
    fn desktop_key_absent_defaults_to_empty_string() {
        // v1 时代的 mobileRelay 块没有 desktopKey → 空串(= 未填,中转会拒),
        // 不能因缺字段导致整个 config 解析失败
        let json = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "mobileRelay": {"relayUrl": "wss://relay.example.com"}
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.mobile_relay.unwrap().desktop_key, "");
    }

    #[test]
    fn launchers_absent_gets_claude_and_codex_presets() {
        // 旧 mobileRelay 块无 launchers 字段 → 预置两条
        let json = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "mobileRelay": {"relayUrl": "wss://relay.example.com"}
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        let launchers = config.mobile_relay.unwrap().launchers;
        assert_eq!(launchers.len(), 2);
        assert_eq!(launchers[0].name, "Claude");
        assert_eq!(launchers[0].command, "claude");
        assert!(launchers[0].shell.is_none());
        assert_eq!(launchers[1].name, "Codex");
        assert_eq!(launchers[1].command, "codex");
    }

    #[test]
    fn migration_fills_missing_mobile_relay_block_with_presets() {
        // 整块 mobileRelay 缺失(从未用过移动端)→ 迁移补一份缺省,面板一打开就有启动器
        let config: AppConfig = serde_json::from_str(
            r#"{"projects":[],"defaultShell":"cmd","availableShells":[],"uiFontSize":13,"terminalFontSize":14}"#,
        )
        .unwrap();
        let migrated = migrate_config(config);
        let relay = migrated.mobile_relay.expect("迁移后应补上 mobileRelay");
        assert_eq!(relay.launchers.len(), 2);
        assert_eq!(relay.relay_url, "");
        assert_eq!(relay.desktop_key, "");
    }

    #[test]
    fn migration_keeps_deliberately_emptied_launcher_list() {
        // 用户把启动器删光是有意结果,迁移不能"好心"把预置塞回去
        let config: AppConfig = serde_json::from_str(
            r#"{"projects":[],"defaultShell":"cmd","availableShells":[],"uiFontSize":13,
                "terminalFontSize":14,"mobileRelay":{"relayUrl":"","desktopKey":"","launchers":[]}}"#,
        )
        .unwrap();
        let migrated = migrate_config(config);
        assert!(migrated.mobile_relay.unwrap().launchers.is_empty());
    }

    #[test]
    fn launcher_round_trip_keeps_optional_shell() {
        // shell 绑定("在 WSL bash 里跑 claude")与留空两种形态都要往返保真
        let launchers = vec![
            AiLauncher {
                id: "l1".into(),
                name: "Claude (WSL)".into(),
                shell: Some("wsl-bash".into()),
                command: "claude".into(),
            },
            AiLauncher {
                id: "l2".into(),
                name: "Codex".into(),
                shell: None,
                command: "codex --model gpt-5".into(),
            },
        ];
        let json = serde_json::to_string(&launchers).unwrap();
        assert!(json.contains(r#""shell":"wsl-bash""#), "{json}");
        assert_eq!(
            json.matches("shell").count(),
            1,
            "未绑定 shell 的启动器不应序列化该字段: {json}"
        );
        let parsed: Vec<AiLauncher> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, launchers);
    }

    #[test]
    fn legacy_cc_connect_field_is_ignored_and_dropped_on_save() {
        // cc-connect 集成已移除:带 ccConnect 字段的旧 config.json 必须静默加载
        // (serde 默认忽略未知字段),且重新序列化后该字段消失(升级无感自动清除)。
        let json = r#"{
            "projects": [],
            "defaultShell": "cmd",
            "availableShells": [],
            "uiFontSize": 13,
            "terminalFontSize": 14,
            "ccConnect": {
                "exePath": "C:\\tools\\cc-connect.exe",
                "configPath": "",
                "autoStart": true,
                "extraArgs": ["--verbose"],
                "projectLinks": {"p1": "proj-one"}
            }
        }"#;
        let config: AppConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.default_shell, "cmd");

        let serialized = serde_json::to_string(&config).unwrap();
        assert!(
            !serialized.contains("ccConnect"),
            "保存后不应残留 ccConnect 字段: {serialized}"
        );
    }

    #[test]
    fn nested_tree_round_trip() {
        let tree = vec![
            ProjectTreeItem::ProjectId("p1".into()),
            ProjectTreeItem::Group(ProjectGroup {
                id: "g1".into(),
                name: "Group1".into(),
                collapsed: false,
                children: vec![
                    ProjectTreeItem::ProjectId("p2".into()),
                    ProjectTreeItem::Group(ProjectGroup {
                        id: "g2".into(),
                        name: "Sub".into(),
                        collapsed: true,
                        children: vec![ProjectTreeItem::ProjectId("p3".into())],
                    }),
                ],
            }),
        ];
        let json = serde_json::to_string(&tree).unwrap();
        let parsed: Vec<ProjectTreeItem> = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.len(), 2);
    }

    // --- 写盘令牌(原 Tauri managed state ConfigToken 的等价物)---

    #[test]
    fn save_rejected_without_load() {
        // 没 load 过 → 令牌 0 → 恒拒绝,磁盘上不该出现 config.json
        let root = unique_test_root("token-never-loaded");
        let store = ConfigStore::at(root.join("config.json"));
        assert_eq!(store.current_token(), 0);
        let err = store.save(0, &AppConfig::default()).unwrap_err();
        assert!(matches!(err, SaveError::StaleToken { .. }));
        assert!(!store.path().exists());
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn save_accepts_fresh_token_and_stale_one_is_rejected() {
        let root = unique_test_root("token-rotate");
        let store = ConfigStore::at(root.join("config.json"));

        let first = store.load().unwrap();
        assert_eq!(first.token, 1);
        store
            .save(
                first.token,
                &AppConfig {
                    default_shell: "first".into(),
                    ..AppConfig::default()
                },
            )
            .unwrap();

        // 别处重新加载 → 令牌轮换 → 老令牌立即作废(后写者必须重读)
        let second = store.load().unwrap();
        assert_eq!(second.token, 2);
        assert_eq!(second.config.default_shell, "first");
        let err = store.save(first.token, &AppConfig::default()).unwrap_err();
        assert!(
            matches!(
                err,
                SaveError::StaleToken {
                    provided: 1,
                    current: 2
                }
            ),
            "{err}"
        );
        // 被拒的那次不该动磁盘
        assert_eq!(store.read().default_shell, "first");
        fs::remove_dir_all(&root).ok();
    }

    /// 配置落的是 `config.db`,`config.json` 只剩 SSH 投影;每次 load 留一代库备份。
    #[test]
    fn 配置落库而_config_json_只剩投影() {
        let root = unique_test_root("save-to-db");
        let store = ConfigStore::at(root.join("config.json"));
        let token = store.load().unwrap().token;

        store
            .save(
                token,
                &AppConfig {
                    default_shell: "gen1".into(),
                    ui_font_size: 17.0,
                    ..AppConfig::default()
                },
            )
            .unwrap();

        assert!(root.join("config.db").exists(), "配置本体落在库里");
        assert!(root.join("config.db.bak").exists(), "load 后留一代库备份");
        assert_eq!(store.read().default_shell, "gen1");
        assert_eq!(store.read().ui_font_size, 17.0);

        // config.json 是投影:只有那两个键,一个设置字段都不该有
        let json = fs::read_to_string(root.join("config.json")).unwrap();
        let projection: serde_json::Value = serde_json::from_str(&json).unwrap();
        // 排序后比较:serde_json 在本工作区的 feature 统一下开了 `preserve_order`
        // (Map 是 IndexMap、保插入序),单独构建 mt-config 时却是 BTreeMap 的字典序
        // —— 断言键顺序会随「跟谁一起编译」而漂。
        let mut keys: Vec<&str> = projection
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["projects", "sshConnections"],
            "投影只该有这两个键: {json}"
        );
        assert!(
            !json.contains("defaultShell"),
            "设置不该出现在投影里: {json}"
        );
        assert!(!json.contains("uiFontSize"));

        fs::remove_dir_all(&root).ok();
    }

    /// 存量 config.json 首次启动被整份迁进库,原文件另存为 `.pre-sqlite`,
    /// 且**只迁一次** —— 迁完 config.json 就被投影覆盖,再迁一次会把库清成投影那点内容。
    #[test]
    fn 存量配置迁入库且只迁一次() {
        let root = unique_test_root("import-once");
        let path = root.join("config.json");
        let legacy = serde_json::json!({
            "projects": [
                {"id": "p1", "name": "甲", "path": "D:/a", "sshMcpEnabled": true,
                 "sshCliToken": "tok-a", "sshConnectionIds": ["c1"]},
                {"id": "p2", "name": "乙", "path": "D:/b"}
            ],
            "defaultShell": "cmd",
            "availableShells": [{"name": "cmd", "command": "cmd.exe"}],
            "uiFontSize": 15.5,
            "theme": "dark",
            "sshConnections": [
                {"id": "c1", "name": "prod", "host": "h1", "port": 22, "user": "root",
                 "password": "secret"},
                {"id": "c2", "name": "dev", "host": "h2", "port": 2222, "user": "deploy"}
            ]
        });
        fs::write(&path, serde_json::to_string_pretty(&legacy).unwrap()).unwrap();

        let store = ConfigStore::at(&path);
        let loaded = store.load().unwrap();
        assert_eq!(loaded.config.projects.len(), 2);
        assert_eq!(loaded.config.projects[0].name, "甲");
        assert_eq!(loaded.config.ui_font_size, 15.5);
        assert_eq!(loaded.config.theme, "dark");
        assert_eq!(loaded.config.ssh_connections.len(), 2);

        // 原文件存档,内容仍是完整的旧配置
        let archived = fs::read_to_string(root.join("config.json.pre-sqlite")).unwrap();
        assert!(archived.contains("uiFontSize"), "存档必须是完整旧配置");

        // 二次 load 走库,不再碰 config.json(此时它已是投影)
        let again = store.load().unwrap();
        assert_eq!(again.config.projects.len(), 2, "二次加载仍拿得到全部项目");
        assert_eq!(again.config.ui_font_size, 15.5);
        assert_eq!(again.config.theme, "dark");

        fs::remove_dir_all(&root).ok();
    }

    /// **投影必须让 sidecar 读得懂**。这里直接调 `mt-core` 那份解析器
    /// ——它就是三个 sidecar 二进制在用的同一段代码,两边隔着 crate 边界、
    /// 没有共享类型,只靠字段名对齐。少写一个 `sshCliToken` 就是 SSH 工具
    /// 集体鉴权失败,而那种故障只在装机后才暴露。
    #[test]
    fn 投影能被_sidecar_的解析器读懂() {
        let root = unique_test_root("projection-sidecar");
        let path = root.join("config.json");
        let store = ConfigStore::at(&path);
        let token = store.load().unwrap().token;

        let config = AppConfig {
            ssh_connections: vec![
                SshConnection {
                    id: "c1".into(),
                    name: "prod".into(),
                    host: "h1".into(),
                    port: 22,
                    user: "root".into(),
                    password: Some("secret".into()),
                    identity_file: None,
                    group: None,
                },
                SshConnection {
                    id: "c2".into(),
                    name: "dev".into(),
                    host: "h2".into(),
                    port: 2222,
                    user: "deploy".into(),
                    password: None,
                    identity_file: None,
                    group: None,
                },
            ],
            projects: vec![
                ProjectConfig {
                    id: "p1".into(),
                    name: "甲".into(),
                    path: "D:/a".into(),
                    ssh_mcp_enabled: true,
                    ssh_cli_token: Some("tok-a".into()),
                    ssh_connection_ids: Some(vec!["c1".into()]),
                    ..project_stub()
                },
                ProjectConfig {
                    id: "p2".into(),
                    name: "乙".into(),
                    path: "D:/b".into(),
                    ..project_stub()
                },
            ],
            ..Default::default()
        };
        store.save(token, &config).unwrap();

        // 能力令牌 → 该项目的连接范围
        let scoped =
            mt_core::read_ssh_connections_for_token_at(Some(path.clone()), "tok-a").unwrap();
        let ids: Vec<&str> = scoped.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["c1"], "令牌必须解析到该项目的范围");
        assert_eq!(scoped[0].password.as_deref(), Some("secret"), "凭据要完整");

        // 未知令牌仍 fail closed
        assert!(mt_core::read_ssh_connections_for_token_at(Some(path.clone()), "nope").is_err());

        // MCP 那条路:按 project-id 取范围;没设范围的项目看得到全部
        let by_id = mt_core::read_ssh_connections_for_project_at(Some(path.clone()), Some("p1"));
        assert_eq!(by_id.len(), 1);
        let unscoped = mt_core::read_ssh_connections_for_project_at(Some(path), Some("p2"));
        assert_eq!(unscoped.len(), 2, "没设范围的项目仍是全部可见");

        fs::remove_dir_all(&root).ok();
    }

    fn project_stub() -> ProjectConfig {
        ProjectConfig {
            id: String::new(),
            name: String::new(),
            path: String::new(),
            description: None,
            saved_layout: None,
            expanded_dirs: vec![],
            ssh_mcp_enabled: false,
            ssh_cli_token: None,
            ssh_connection_ids: None,
            env_vars: vec![],
            wsl_sessions_distro: None,
            ssh_connection_id: None,
            parent_project_id: None,
            kind_override: None,
        }
    }

    #[test]
    fn read_falls_back_to_default_without_touching_token() {
        let root = unique_test_root("read-tolerant");
        let store = ConfigStore::at(root.join("config.json"));
        fs::write(store.path(), "{ corrupted").unwrap();
        // 主+备均不可用 → 按默认配置启动,且不发令牌(默认配置永远拿不到写盘资格)
        assert!(!store.read().available_shells.is_empty());
        assert_eq!(store.current_token(), 0);
        assert!(store.load().is_err());
        assert_eq!(store.current_token(), 0);
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let root = unique_test_root("atomic-write");
        let path = root.join("x.json");
        atomic_write(&path, b"hello").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        atomic_write(&path, b"world").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "world");
        let leftovers: Vec<_> = fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "x.json")
            .collect();
        assert!(leftovers.is_empty(), "残留临时文件: {leftovers:?}");
        fs::remove_dir_all(&root).ok();
    }
}
