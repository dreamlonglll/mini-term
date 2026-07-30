use mt_core::SshConnection;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager};

/// 历史 identifier。0.2.20 及之前版本使用模板默认值,从 0.2.21 开始切换到
/// `com.mini-term.app`。首次启动时一次性把旧目录下的 config.json 拷到新目录,
/// 旧文件保留不删,作为回退兜底。
const LEGACY_IDENTIFIER: &str = "com.tauri-app.tauri-app";

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
    #[serde(default)]
    pub ssh_connections: Vec<SshConnection>,
    /// 显式创建的 SSH 分组名（允许空分组存在）。连接上的 group 字段仍是归属的
    /// 单一来源，此列表只补充「还没有连接的分组」；空 Vec 时序列化跳过。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ssh_groups: Vec<String>,
    /// 移动端中转配置(docs/adr/0001)。None = 未启用;序列化时省略保持文件干净。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mobile_relay: Option<MobileRelayConfig>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
pub enum SavedSplitNode {
    Leaf {
        /// 旧格式（单个 pane），仅用于反序列化兼容，序列化时跳过
        #[serde(default, skip_serializing)]
        pane: Option<SavedPane>,
        /// 新格式（pane 数组），前端始终使用此字段
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
    #[serde(default)]
    pub saved_layout: Option<SavedProjectLayout>,
    #[serde(default)]
    pub expanded_dirs: Vec<String>,
    /// 是否已为该项目启用 SSH MCP（向项目目录写入了 Claude / Codex 的 MCP 注册配置）。
    #[serde(default)]
    pub ssh_mcp_enabled: bool,
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
            layout_sizes: None,
            middle_column_sizes: None,
            theme: default_theme(),
            skin: default_skin(),
            terminal_follow_theme: default_terminal_follow_theme(),
            ai_completion_popup: default_ai_completion_popup(),
            ai_completion_taskbar_flash: default_ai_completion_taskbar_flash(),
            ai_completion_sound: true,
            ai_completion_sound_path: None,
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
            ssh_connections: vec![],
            ssh_groups: vec![],
            mobile_relay: None,
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

fn config_path(app: &AppHandle) -> PathBuf {
    let dir = app
        .path()
        .app_data_dir()
        .expect("failed to get app data dir");
    fs::create_dir_all(&dir).ok();
    dir.join("config.json")
}

/// 纯函数版本,接收新 app_data_dir 路径。便于单元测试。
///
/// 行为:
/// 1. 新目录已有 `config.json` → 直接返回(已迁移过 / 全新用户首次保存生成)
/// 2. 新目录无 `config.json`,但老 identifier 目录有 → 拷过来
/// 3. 老目录也没有 → 返回(全新安装)
///
/// 老 config.json 不删除,作为回退兜底。create_dir_all / copy 失败仅打印日志,
/// 不 panic — 后续 read_config 会在缺文件时退化为 default。
fn migrate_app_data_at(new_dir: &Path) {
    let new_config = new_dir.join("config.json");
    if new_config.exists() {
        return;
    }
    let Some(base_dir) = new_dir.parent() else {
        return;
    };
    let old_config = base_dir.join(LEGACY_IDENTIFIER).join("config.json");
    if !old_config.exists() {
        return;
    }
    if let Err(e) = fs::create_dir_all(new_dir) {
        eprintln!("[migrate] 创建新数据目录失败 {}: {e}", new_dir.display());
        return;
    }
    match fs::copy(&old_config, &new_config) {
        Ok(_) => {
            eprintln!(
                "[migrate] 已将旧 config.json 迁移至新目录: {}",
                new_config.display()
            );
        }
        Err(e) => {
            eprintln!("[migrate] 拷贝旧 config.json 失败: {e}");
        }
    }
}

/// 在 lib.rs setup 早期调用,保证所有 read_config 之前完成 identifier 迁移。
pub fn migrate_legacy_app_data(app: &AppHandle) {
    if let Ok(new_dir) = app.path().app_data_dir() {
        migrate_app_data_at(&new_dir);
    }
}

/// 将旧格式 `pane`（单个）迁移到新格式 `panes`（数组）
fn normalize_split_node(node: &mut SavedSplitNode) {
    match node {
        SavedSplitNode::Leaf { pane, panes } => {
            if let Some(p) = pane.take() {
                if panes.is_empty() {
                    panes.push(p);
                }
            }
        }
        SavedSplitNode::Split { children, .. } => {
            for child in children.iter_mut() {
                normalize_split_node(child);
            }
        }
    }
}

fn migrate_config(mut config: AppConfig) -> AppConfig {
    // 迁移 vscodePath → editors
    if config.editors.is_empty() {
        if let Some(ref path) = config.vscode_path {
            let trimmed = path.trim();
            if !trimmed.is_empty() {
                config.editors.push(EditorConfig {
                    name: "VS Code".into(),
                    command: trimmed.into(),
                });
                config.default_editor = Some("VS Code".into());
            }
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

/// 从磁盘加载并迁移配置。供后端内部调用(例如 editor.rs 读取 vscode_path)。
pub fn read_config(app: &AppHandle) -> AppConfig {
    let path = config_path(app);
    match fs::read_to_string(&path) {
        Ok(content) => migrate_config(serde_json::from_str(&content).unwrap_or_default()),
        Err(_) => migrate_config(AppConfig::default()),
    }
}

#[tauri::command]
pub fn load_config(app: AppHandle) -> AppConfig {
    read_config(&app)
}

#[tauri::command]
pub fn save_config(app: AppHandle, config: AppConfig) -> Result<(), String> {
    let path = config_path(&app);
    let json = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    // 原子写,避免写入中途崩溃留下截断的 config.json 导致全部用户配置丢失
    crate::fs::atomic_write(&path, json.as_bytes()).map_err(|e| e.to_string())
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
                            }],
                        },
                        SavedSplitNode::Leaf {
                            pane: None,
                            panes: vec![SavedPane {
                                shell_name: "powershell".into(),
                                cwd: None,
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
    fn migrate_copies_legacy_config_when_new_dir_empty() {
        let root = unique_test_root("migrate-copy");
        let new_dir = root.join("com.mini-term.app");
        let old_dir = root.join(LEGACY_IDENTIFIER);
        fs::create_dir_all(&old_dir).unwrap();
        let payload = r#"{"projects":[],"defaultShell":"cmd","availableShells":[]}"#;
        fs::write(old_dir.join("config.json"), payload).unwrap();

        migrate_app_data_at(&new_dir);

        let migrated = fs::read_to_string(new_dir.join("config.json")).unwrap();
        assert_eq!(migrated, payload);
        // 旧文件保留作为兜底
        assert!(old_dir.join("config.json").exists());

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn migrate_skips_when_new_config_already_exists() {
        let root = unique_test_root("migrate-skip-exists");
        let new_dir = root.join("com.mini-term.app");
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(new_dir.join("config.json"), "current").unwrap();

        let old_dir = root.join(LEGACY_IDENTIFIER);
        fs::create_dir_all(&old_dir).unwrap();
        fs::write(old_dir.join("config.json"), "legacy").unwrap();

        migrate_app_data_at(&new_dir);

        let after = fs::read_to_string(new_dir.join("config.json")).unwrap();
        assert_eq!(after, "current");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn migrate_noop_when_legacy_missing() {
        let root = unique_test_root("migrate-noop");
        let new_dir = root.join("com.mini-term.app");

        migrate_app_data_at(&new_dir);

        // 没有任何东西被创建
        assert!(!new_dir.join("config.json").exists());

        fs::remove_dir_all(&root).ok();
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
        assert_eq!(config.projects[0].ssh_connection_id.as_deref(), Some("conn-1"));
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
        assert_eq!(reparsed.projects[0].ssh_connection_id.as_deref(), Some("conn-1"));
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
}
