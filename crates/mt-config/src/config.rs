//! `AppConfig` 的 schema、迁移与磁盘读写。
//!
//! 与 Tauri 版的差别只有两处出入口:路径不再来自 `AppHandle`(见 [`crate::paths`]),
//! 写盘令牌不再是 Tauri managed state 而是 [`ConfigStore`] 自己的字段。
//! **序列化面一个字段都没动** —— 存量 `config.json` 必须原样读得进来。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};

/// SSH 连接。
///
/// **临时定义**:原本来自 `src-tauri/mt-core::SshConnection`,而 `mt-core` 按
/// `docs/gpui-migration.md` 第 7 节要留在 `src-tauri/` 下直到 `remote_ssh.rs`
/// 迁移(它还被 `mt-sidecars` 引用,提前挪会打断 sidecar 构建管线)。
/// 这里逐字段复刻同一份 serde 形状(camelCase + 全部 Option 带 default),
/// 保证 `config.json` 双向兼容。
/// **TODO**:`mt-core` 物理移入 `crates/` 后,本类型改为 `pub use mt_core::SshConnection`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SshConnection {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

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
    #[serde(default)]
    pub layout_sizes: Option<Vec<f64>>,
    #[serde(default)]
    pub middle_column_sizes: Option<Vec<f64>>,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_skin")]
    pub skin: String,
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
    // NOTE: 曾有 projects_visible / sessions_visible / files_visible / git_visible
    // 四个面板显隐开关，界面上没有任何入口消费（已被 middle_column_visible 与右侧
    // 抽屉取代），随 UI 改版一并删除。旧 config.json 里残留的这些键会被 serde 忽略。
    #[serde(default = "default_true")]
    pub middle_column_visible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
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
    /// 激活时 theme/skin 保持不动，退出自定义主题可无损回落。
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
    #[serde(default)]
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
fn default_skin() -> String {
    "none".into()
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
            skin: default_skin(),
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
            middle_column_visible: true,
            right_drawer_width: None,
            last_active_project_id: None,
            hook_enabled: false,
            smart_copy_paste: false,
            selection_auto_copy_secs: None,
            tray_status_enabled: None,
            tray_max_projects: None,
            tray_click_focus: None,
            ai_auto_resume: None,
            ssh_connections: vec![],
            ssh_groups: vec![],
            mobile_relay: None,
            custom_theme_id: None,
            session_list_view: None,
            session_lineage: vec![],
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
            for tab in layout.tabs.iter_mut() {
                normalize_split_node(&mut tab.split_layout);
            }
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

/// save 前是否把现有主文件留作 .bak:仅内容仍可解析时才值得备份,
/// 损坏的主文件绝不覆盖仍有抢救价值的上一代备份。
fn should_backup(existing: Option<&str>) -> bool {
    existing.is_some_and(|c| serde_json::from_str::<AppConfig>(c).is_ok())
}

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
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::StaleToken { .. } => None,
            Self::Serialize(e) => Some(e),
            Self::Io(e) => Some(e),
        }
    }
}

/// `config.json` 的读写口,同时持有**写盘令牌**。
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
    path: PathBuf,
    token: AtomicU64,
}

impl ConfigStore {
    /// 指向 `{app_data_dir}/config.json`,并顺手跑一次 identifier 迁移
    /// ——迁移必须早于任何一次读取,放在这里就无法忘记。
    pub fn open() -> Result<Self> {
        crate::paths::migrate_legacy_app_data();
        Ok(Self::at(crate::paths::config_path()?))
    }

    /// 指向任意路径(测试与"导入/导出配置"用)。
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            token: AtomicU64::new(0),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 当前有效令牌。0 = 还没有过一次成功的 [`load`](Self::load)。
    pub fn current_token(&self) -> u64 {
        self.token.load(Ordering::Acquire)
    }

    /// 严格加载:文件不存在 = 首次启动,正常返回默认配置;
    /// 文件存在但读不出/解析失败 = 先尝试用上一代备份 .bak 自愈,
    /// 备份也不行才返回错误——绝不把默认空配置伪装成加载成功
    /// (那会让调用方拿着空配置开始运行,下一次保存就把磁盘覆盖了)。
    ///
    /// 加载成功才轮换发放令牌;上一轮的令牌随之作废。
    pub fn load(&self) -> Result<LoadedConfig> {
        let config = match read_config_from(&self.path)? {
            Some(config) => config,
            None => migrate_config(AppConfig::default()),
        };
        let token = self.token.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        eprintln!("[config] load ok, token={token}");
        Ok(LoadedConfig { config, token })
    }

    /// 容错加载:任何读取/解析失败都回退默认,且**不轮换令牌**。
    ///
    /// 供后台路径(hook / relay / 新建 PTY 取项目 env)只读取个别字段用——
    /// 它们不写盘,吞错无害;后台必须能启动,只在主+备均不可用时才按默认运行。
    pub fn read(&self) -> AppConfig {
        match read_config_from(&self.path) {
            Ok(Some(config)) => config,
            Ok(None) => migrate_config(AppConfig::default()),
            Err(e) => {
                eprintln!("[config] {e}; 后台本次按默认配置启动");
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
        let json = serde_json::to_string_pretty(config).map_err(SaveError::Serialize)?;
        // 内容没变不写盘,也避免用相同内容覆盖掉仍有抢救价值的 .bak
        let existing = fs::read_to_string(&self.path).ok();
        if existing.as_deref() == Some(json.as_str()) {
            return Ok(());
        }
        // 覆写前留一代备份,任何原因导致配置被写坏都可救回。
        // 仅当现有主文件仍可解析时才备份——损坏的主文件绝不覆盖仍有抢救价值的 .bak
        if should_backup(existing.as_deref()) {
            let _ = fs::copy(&self.path, self.path.with_extension("json.bak"));
        }
        // 原子写,避免写入中途崩溃留下截断的 config.json 导致全部用户配置丢失
        atomic_write(&self.path, json.as_bytes()).map_err(SaveError::Io)
    }
}

/// 同目录临时文件 + rename 的原子写。
///
/// 与 `src-tauri/src/fs.rs::atomic_write`(去向 `mt-project`)是同一份实现的副本:
/// 配置写盘是本 crate 的核心不变量,不能为了一个 20 行的工具函数把 mt-config
/// 挂到 mt-project 上(依赖方向会倒过来)。
/// **TODO**:等工作区出现共享工具 crate 后合并成一份。
fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "目标路径没有父目录")
    })?;
    // 临时文件必须与目标同目录,保证同卷,rename 才能原子
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("tmp");
    let tmp = dir.join(format!(".{}.{}.{}.tmp", stem, std::process::id(), seq));

    let write_result = (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.flush()?;
        let _ = f.sync_all(); // sync 失败不致命,尽力而为
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }

    // 若目标已存在,把其权限位复制到临时文件,避免 rename 后权限退化为 umask 默认值
    // (Unix 下保护用户 chmod 600 的含 token 配置不被降级为 0644;Windows 上对应只读位)。
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }

    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

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

    #[test]
    fn corrupted_main_never_backed_up() {
        assert!(!should_backup(Some("{ corrupted")));
        assert!(!should_backup(None));
        let valid = serde_json::to_string(&AppConfig::default()).unwrap();
        assert!(should_backup(Some(&valid)));
    }

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

    #[test]
    fn save_keeps_previous_generation_as_backup() {
        let root = unique_test_root("save-backup");
        let store = ConfigStore::at(root.join("config.json"));
        let token = store.load().unwrap().token;
        store
            .save(
                token,
                &AppConfig {
                    default_shell: "gen1".into(),
                    ..AppConfig::default()
                },
            )
            .unwrap();
        // 同一份内容再存一次:不写盘、也不产生备份
        store
            .save(
                token,
                &AppConfig {
                    default_shell: "gen1".into(),
                    ..AppConfig::default()
                },
            )
            .unwrap();
        assert!(!root.join("config.json.bak").exists());

        store
            .save(
                token,
                &AppConfig {
                    default_shell: "gen2".into(),
                    ..AppConfig::default()
                },
            )
            .unwrap();
        let bak: AppConfig =
            serde_json::from_str(&fs::read_to_string(root.join("config.json.bak")).unwrap())
                .unwrap();
        assert_eq!(bak.default_shell, "gen1");
        assert_eq!(store.read().default_shell, "gen2");
        fs::remove_dir_all(&root).ok();
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
