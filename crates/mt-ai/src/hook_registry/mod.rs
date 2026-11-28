//! Hook 注册/卸载模块
//!
//! 一键把 Claude Code / Codex / Grok / oh-my-pi / pi / Antigravity CLI 各家的 hook 配置写进
//! 各自的配置文件，并提供配置片段供用户手动粘贴。原本是一组 Tauri command，迁移后是普通函数。
//!
//! # Antigravity CLI（agy）
//!
//! 写 `~/.gemini/config/hooks.json` 里 mini-term 自己的组（别家的组原样保留），只注册
//! `PreInvocation` / `Stop` 两个事件，**绝不注册 `PreToolUse`**（sidecar 对 agy 输出
//! `{}`，那在 `PreToolUse` 上等于拒绝工具）。细节见 agy 段的注释与 `AGY_HOOK_EVENTS`。
//!
//! # oh-my-pi（omp）与 pi 不走 sidecar
//!
//! 这两家没有「shell 命令 hook」这种东西：扩展点是**进程内加载的 TS 模块**
//! （`{agent_dir}/extensions/*.ts`，omp 跑在 Bun 上、pi 跑在 Node 上经 jiti 加载）。
//! 所以不注册二进制命令，而是把一份自带的 TS 扩展（[`OMP_EXTENSION_SOURCE`] /
//! [`PI_EXTENSION_SOURCE`]）整份写进该目录；扩展在 CLI 进程内直接 `fetch` 本地 hook
//! 服务器，事件名翻译成与 Claude 同名的 PascalCase 后 POST，hook server 不用为它们
//! 改一行。详见 [`TsExtension`]。
//!
//! # Grok 的两处结构性差异（改动前先读 [`register_grok_hooks`] 与
//! [`GROK_HOOK_FILE`] 的注释）
//!
//! 1. grok 默认还会扫描 `~/.claude/settings.json` 的 hooks（Claude 兼容层），
//!    同一事件会来两趟。sidecar 靠 `GROK_SESSION_ID` + 是否带 argv 丢弃兼容层
//!    那趟，判据落在**原生 hook 文件是否在场**（即 `{grok_home}/hooks/miniterm.json`
//!    这个文件名，两处必须一致）——只注册了 Claude 的用户必须放行，那是他们
//!    唯一的来源。
//! 2. 注册进 `~/.grok/hooks/` 的命令必须是**不含空格的裸文件名**（hook 二进制
//!    随注册复制进该目录）。带空格会被 grok 丢给 shell，而 Windows 上具体是
//!    git-bash / pwsh / powershell / cmd 由环境决定、四家引号语义互斥。
//!    事件名改由 grok 注入的 `GROK_HOOK_EVENT` 传递。

use crate::agent::AgentKind;
use crate::claude_settings;
use serde_json::Value;
use std::path::PathBuf;

/// miniterm-hook 命令的标识符，用于检测和更新已存在的 hook 条目
const HOOK_MARKER: &str = "miniterm-hook";

/// Claude Code 需要注册的 hook 事件列表
///
/// 事件名是白名单：Claude Code 只对认识的事件派发，settings.json 里多出的
/// 事件名被忽略，所以列表可以领先于用户的 Claude Code 版本，不会让旧版报错。
const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    // 工具失败后 AI 仍在处理错误。只注册 PostToolUse 会漏掉整个失败分支，
    // 状态要等到下一个 PreToolUse 才恢复。
    "PostToolUseFailure",
    // 一批并行工具全部结束、下次模型调用之前。并行工具批场景下它是唯一
    // 覆盖「批已收尾但模型还没被调用」这段的事件。
    "PostToolBatch",
    "Stop",
    // 回合因 API 错误结束。官方文档：`Stop` 在这种情况下不触发
    // （"API errors fire StopFailure instead"）——不注册它，限流/超载/鉴权失败
    // 之后 pane 会确定性地卡在 ai-working 直到下一轮对话。
    "StopFailure",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
    "PermissionRequest",
    // auto 模式分类器拒绝了工具调用。拒绝后 AI 继续处理，同时它是权限黄灯的
    // 熄灭路径之一（状态转回 ai-working 会清掉 attention）。
    "PermissionDenied",
    "Notification",
    "Elicitation",
    // 用户回应了 MCP 表单 → AI 继续。与 Elicitation 成对，缺了它黄灯要等到
    // 下一个工具事件才熄。
    "ElicitationResult",
];

/// Codex 需要注册的 hook 事件列表
const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    // monitor 的 hook 权威模式只接受 SessionEnd 作为会话退出信号。
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "PermissionRequest",
];

/// Grok Build 需要注册的 hook 事件列表（官方事件表的全集）。
///
/// 与 Claude 的差异：没有 `PermissionRequest` / `PostToolBatch` /
/// `Elicitation`——「等待授权」走 `Notification` 的 `permission_prompt` 类型，
/// 由 `hook_server::classify_notification` 归一化成同一盏黄灯。
/// 事件名写 PascalCase：grok 的事件表把它列为合法别名，且与另外两家对齐。
const GROK_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "Stop",
    "StopFailure",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
];

/// Grok hook 条目的超时（秒）。
///
/// `Stop` / `SubagentStop` 在 grok 里是**阻塞闸**（默认 600s），闸内跑的是我们
/// 这个 POST 完就退的小二进制，30s 绰绰有余；真超时 grok 也是 fail-open，
/// 回合照常结束，不会把 AI 卡死。
const GROK_HOOK_TIMEOUT_SECS: u64 = 30;

/// mini-term 写进 grok hooks 目录的配置文件名（sidecar 也按这个名字判断
/// 「原生条目是否在场」以丢弃 Claude 兼容层的重复投递，两处必须一致）
const GROK_HOOK_FILE: &str = "miniterm.json";

/// 该 hook 条目是否由 mini-term 写入。
///
/// Claude 与 Codex 的条目在这一层结构一致：`{ "hooks": [{ "command": "…" }] }`，
/// 按命令文本里的 `miniterm-hook` 标识判定，不碰用户自己的 hook。
fn entry_is_miniterm(entry: &Value) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hooks_arr| {
            hooks_arr.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(|c| c.contains(HOOK_MARKER))
            })
        })
}

/// 获取 miniterm-hook 二进制的绝对路径（与主程序同目录）
pub fn hook_binary_path() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法获取当前程序路径: {}", e))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "无法获取程序所在目录".to_string())?;

    let hook_path = dir.join(hook_binary_name());
    Ok(hook_path.to_string_lossy().to_string())
}

/// 获取 Codex hook 配置文件路径: ~/.codex/hooks.json
fn codex_hooks_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("hooks.json"))
}

/// 获取 Codex 配置文件路径: ~/.codex/config.toml
fn codex_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("config.toml"))
}

/// grok 的用户级配置根目录：`$GROK_HOME` 优先，否则 `~/.grok`
/// （与 grok 自身 `grok_home()` 的口径一致）
pub fn grok_home() -> Option<PathBuf> {
    match std::env::var("GROK_HOME") {
        Ok(h) if !h.is_empty() => Some(PathBuf::from(h)),
        _ => dirs::home_dir().map(|h| h.join(".grok")),
    }
}

/// grok hooks 目录：`{grok_home}/hooks`
fn grok_hooks_dir() -> Option<PathBuf> {
    grok_home().map(|h| h.join("hooks"))
}

/// mini-term 写入的 grok hook 配置文件路径
fn grok_hooks_path() -> Option<PathBuf> {
    grok_hooks_dir().map(|d| d.join(GROK_HOOK_FILE))
}

/// hook 二进制在 grok hooks 目录里的副本路径
fn grok_hook_binary_path() -> Option<PathBuf> {
    grok_hooks_dir().map(|d| d.join(hook_binary_name()))
}

fn hook_binary_name() -> &'static str {
    if cfg!(windows) {
        "miniterm-hook.exe"
    } else {
        "miniterm-hook"
    }
}

mod agy;
mod claude;
mod codebuddy;
mod codex;
mod grok;
mod ts_ext;

pub use agy::sync_agy_hooks_if_registered;
pub use claude::sync_claude_hooks_if_registered;
pub use codex::sync_codex_hooks_feature_if_registered;
pub use grok::sync_grok_hooks_if_registered;
pub use ts_ext::{
    OMP_REPORTED_EVENTS, PI_REPORTED_EVENTS, omp_agent_dir, pi_agent_dir,
    sync_omp_hooks_if_registered, sync_pi_hooks_if_registered,
};

use agy::*;
use claude::*;
use codex::*;
use grok::*;
use ts_ext::*;

// ─── 注册目标 ───

/// 可单独注册/卸载的 CLI。
///
/// 各家的事件集、配置文件位置与命令写法都不同（见各自的 `register_*`），但对外
/// 是同一套动作，所以用本枚举做选择而不是铺几对命令：调用方只传一个列表，将来
/// 再加一家时签名不变。serde 层拒收未知值——未知 agent 若静默退化成
/// 「全量注册」，会往用户根本没在用的 CLI 里写配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookAgent {
    Claude,
    Codex,
    Grok,
    /// oh-my-pi（omp）：写的是 TS 扩展而不是 hook 命令，见模块注释
    Omp,
    /// pi（earendil-works/pi，omp 的上游）：同 omp 写 TS 扩展，模板另一份
    Pi,
    /// Antigravity CLI（agy）：写 `~/.gemini/config/hooks.json` 里我们自己的组
    Agy,
    /// CodeBuddy Code：写 `~/.codebuddy/settings.json` 的 hooks 段（Claude 同构）
    CodeBuddy,
}

impl HookAgent {
    pub const ALL: &'static [HookAgent] = &[
        HookAgent::Claude,
        HookAgent::Codex,
        HookAgent::Grok,
        HookAgent::Omp,
        HookAgent::Pi,
        HookAgent::Agy,
        HookAgent::CodeBuddy,
    ];

    /// 注册目标对应的 agent。标识与展示名都查 [`crate::agent`] 那张表,
    /// 这里只管「哪几家有 hook、各自怎么注册」。
    pub fn agent(self) -> AgentKind {
        match self {
            HookAgent::Claude => AgentKind::Claude,
            HookAgent::Codex => AgentKind::Codex,
            HookAgent::Grok => AgentKind::Grok,
            HookAgent::Omp => AgentKind::Omp,
            HookAgent::Pi => AgentKind::Pi,
            HookAgent::Agy => AgentKind::Agy,
            HookAgent::CodeBuddy => AgentKind::CodeBuddy,
        }
    }

    /// 稳定标识(= serde 名 = 设置页勾选项的值)
    pub fn key(self) -> &'static str {
        self.agent().key()
    }

    /// 面板展示名（与 UI 里的品牌写法一致）
    pub fn label(self) -> &'static str {
        self.agent().spec().label
    }

    /// [`Self::key`] 的反查;认不出返回 `None`(不退化成「全量注册」,理由见类型注释)。
    pub fn from_key(key: &str) -> Option<HookAgent> {
        Self::ALL.iter().copied().find(|a| a.key() == key)
    }

    fn events(self) -> &'static [&'static str] {
        match self {
            HookAgent::Claude => CLAUDE_HOOK_EVENTS,
            HookAgent::Codex => CODEX_HOOK_EVENTS,
            HookAgent::Grok => GROK_HOOK_EVENTS,
            HookAgent::Omp => OMP_EXTENSION.events,
            HookAgent::Pi => PI_EXTENSION.events,
            HookAgent::Agy => AGY_HOOK_EVENTS,
            HookAgent::CodeBuddy => codebuddy::CODEBUDDY_HOOK_EVENTS,
        }
    }

    fn registered_events(self) -> std::collections::HashSet<String> {
        match self {
            HookAgent::Claude => registered_claude_events(),
            HookAgent::Codex => registered_codex_events(),
            HookAgent::Grok => registered_grok_events(),
            HookAgent::Omp => OMP_EXTENSION.registered_events(),
            HookAgent::Pi => PI_EXTENSION.registered_events(),
            HookAgent::Agy => registered_agy_events(),
            HookAgent::CodeBuddy => codebuddy::registered_codebuddy_events(),
        }
    }

    /// 配置文件路径的展示形式（`~` 缩写，面板里直接给用户看到写去了哪）
    fn display_path(self) -> String {
        let raw = match self {
            HookAgent::Claude => claude_settings::settings_path(),
            HookAgent::Codex => codex_hooks_path(),
            HookAgent::Grok => grok_hooks_path(),
            HookAgent::Omp => OMP_EXTENSION.path(),
            HookAgent::Pi => PI_EXTENSION.path(),
            HookAgent::Agy => agy_hooks_path(),
            HookAgent::CodeBuddy => codebuddy::codebuddy_settings_path(),
        };
        let Some(raw) = raw else {
            return String::new();
        };
        let text = raw.to_string_lossy().replace('\\', "/");
        match dirs::home_dir() {
            Some(home) => {
                let home = home.to_string_lossy().replace('\\', "/");
                text.strip_prefix(&home)
                    .map(|rest| format!("~{}", rest))
                    .unwrap_or(text)
            }
            None => text,
        }
    }

    fn register(self, hook_path: &str) -> Result<String, String> {
        match self {
            HookAgent::Claude => register_claude_hooks(hook_path),
            HookAgent::Codex => register_codex_hooks(hook_path),
            HookAgent::Grok => register_grok_hooks(hook_path),
            // omp / pi 走的是进程内 TS 扩展，用不着 hook 二进制的路径
            HookAgent::Omp => OMP_EXTENSION.register(),
            HookAgent::Pi => PI_EXTENSION.register(),
            HookAgent::Agy => register_agy_hooks(hook_path),
            HookAgent::CodeBuddy => codebuddy::register_codebuddy_hooks(hook_path),
        }
    }

    fn unregister(self) -> Result<String, String> {
        match self {
            HookAgent::Claude => unregister_claude_hooks(),
            HookAgent::Codex => unregister_codex_hooks(),
            HookAgent::Grok => unregister_grok_hooks(),
            HookAgent::Omp => OMP_EXTENSION.unregister(),
            HookAgent::Pi => PI_EXTENSION.unregister(),
            HookAgent::Agy => unregister_agy_hooks(),
            HookAgent::CodeBuddy => codebuddy::unregister_codebuddy_hooks(),
        }
    }

    /// 与 [`Self::unregister`] 同一套摘除逻辑,结果是摘掉的条目数(omp / pi 是文件数)。
    /// 拿不到 home / 配置目录时按「没有可摘的」返回 0。
    fn remove_entries(self) -> Result<usize, String> {
        match self {
            HookAgent::Claude => claude_settings::settings_path()
                .map_or(Ok(0), |path| remove_claude_entries_at(&path)),
            HookAgent::Codex => {
                codex_hooks_path().map_or(Ok(0), |path| remove_codex_entries_at(&path))
            }
            HookAgent::Grok => grok_hooks_path().map_or(Ok(0), |path| {
                remove_grok_entries_at(&path, grok_hook_binary_path().as_deref())
            }),
            HookAgent::Omp => OMP_EXTENSION.remove_entries(),
            HookAgent::Pi => PI_EXTENSION.remove_entries(),
            HookAgent::Agy => agy_hooks_path().map_or(Ok(0), |path| remove_agy_entries_at(&path)),
            HookAgent::CodeBuddy => codebuddy::codebuddy_settings_path()
                .map_or(Ok(0), |path| codebuddy::remove_codebuddy_entries_at(&path)),
        }
    }
}

/// 入参缺省 / 空列表时的目标：各家全上，保持「一键注册」的原有语义。
fn resolve_targets(agents: Option<Vec<HookAgent>>) -> Vec<HookAgent> {
    match agents {
        Some(list) if !list.is_empty() => {
            // 去重：同一项传两次会把该配置文件写两遍（幂等，但白跑一趟）
            let mut out: Vec<HookAgent> = Vec::new();
            for a in list {
                if !out.contains(&a) {
                    out.push(a);
                }
            }
            out
        }
        _ => HookAgent::ALL.to_vec(),
    }
}

/// 单个 CLI 的 hook 注册现状，供面板显示「装没装 / 是不是旧事件集」。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HookRegistrationInfo {
    /// 与 `HookAgent` 的 serde 表示一致，UI 按它回传选择
    pub agent: String,
    pub label: String,
    /// 配置文件路径（`~` 缩写）
    pub file: String,
    /// 该文件里属于 mini-term 的事件条目数
    pub registered: usize,
    /// 当前版本应注册的事件总数；`0 < registered < total` = 老用户的旧事件集
    pub total: usize,
}

// ─── 对外动作(原 Tauri Commands) ───

/// 注册 AI hooks。`agents` 缺省 = 各家全注册（保持「一键注册」的原语义）。
pub fn register_ai_hooks(agents: Option<Vec<HookAgent>>) -> Result<String, String> {
    let hook_path = hook_binary_path()?;
    let results: Vec<String> = resolve_targets(agents)
        .into_iter()
        .map(|agent| match agent.register(&hook_path) {
            Ok(msg) => msg,
            // 单家失败不打断其余：各家的配置文件互不相干，一家读不动不该
            // 让其余几家也注册不上
            Err(e) => format!("{} 注册失败: {}", agent.label(), e),
        })
        .collect();
    Ok(results.join("\n"))
}

/// 卸载 AI hooks。`agents` 缺省 = 各家全卸载。
pub fn unregister_ai_hooks(agents: Option<Vec<HookAgent>>) -> Result<String, String> {
    let results: Vec<String> = resolve_targets(agents)
        .into_iter()
        .map(|agent| match agent.unregister() {
            Ok(msg) => msg,
            Err(e) => format!("{} 卸载失败: {}", agent.label(), e),
        })
        .collect();
    Ok(results.join("\n"))
}

/// 卸载 mini-term 本体时用(`mini-term.exe --unregister-hooks`,由 NSIS 卸载器调起):
/// 各家([`HookAgent::ALL`])配置里 mini-term 写入的条目全部摘掉,按家返回摘掉的条数。
///
/// 与设置页「卸载」同一套摘除逻辑([`HookAgent::remove_entries`]),只认带
/// `miniterm-hook` 标识的条目 / 文件;没有我们条目的配置文件一个字节不写;单家失败
/// 不影响其余几家。
pub fn purge_all_for_uninstall() -> Vec<(HookAgent, Result<usize, String>)> {
    HookAgent::ALL
        .iter()
        .map(|&agent| (agent, agent.remove_entries()))
        .collect()
}

/// 各家的注册现状（面板据此定默认勾选、显示状态徽章）。
pub fn get_ai_hook_registrations() -> Vec<HookRegistrationInfo> {
    HookAgent::ALL
        .iter()
        .map(|&agent| {
            let events = agent.events();
            let registered = agent.registered_events();
            HookRegistrationInfo {
                agent: agent.key().to_string(),
                label: agent.label().to_string(),
                file: agent.display_path(),
                // 只数当前版本要求的事件：配置里残留的已下线事件名不该
                // 让计数超过 total、显示成「17/16」
                registered: events.iter().filter(|e| registered.contains(**e)).count(),
                total: events.len(),
            }
        })
        .collect()
}

/// TS 扩展片段的落盘位置:与面板那一行同一个路径(跟着 `PI_CODING_AGENT_DIR` 走),
/// 拿不到 home 时退回缺省目录的写法。
fn ts_snippet_file(agent: HookAgent, default_agent_dir: &str) -> String {
    let shown = agent.display_path();
    if shown.is_empty() {
        format!("{default_agent_dir}/extensions/{EXTENSION_FILE}")
    } else {
        shown
    }
}

/// 获取 hook 配置片段供用户手动粘贴（结构化返回）
pub fn get_hook_config_snippet() -> Result<Value, String> {
    let hook_path = hook_binary_path()?;

    // Claude Code 配置片段
    let mut claude_hooks = serde_json::Map::new();
    for event in CLAUDE_HOOK_EVENTS {
        let entry = build_claude_hook_entry(&hook_path, event);
        claude_hooks.insert(event.to_string(), serde_json::json!([entry]));
    }
    let claude_snippet = serde_json::json!({
        "hooks": claude_hooks
    });
    let claude_str = serde_json::to_string_pretty(&claude_snippet).map_err(|e| e.to_string())?;

    // Codex 配置片段 — 镜像 register_codex_hooks 的写入逻辑
    let mut codex_config: Value = serde_json::json!({});
    codex_config["hooks"] = serde_json::json!({});
    if let Some(hooks) = codex_config["hooks"].as_object_mut() {
        for event in CODEX_HOOK_EVENTS {
            hooks.insert(event.to_string(), build_codex_hook_entry(&hook_path, event));
        }
    }
    let codex_str = serde_json::to_string_pretty(&codex_config).map_err(|e| e.to_string())?;

    // Grok 配置片段 — 镜像 register_grok_hooks 的写入逻辑
    let mut grok_config: Value = serde_json::json!({});
    grok_config["hooks"] = serde_json::json!({});
    if let Some(hooks) = grok_config["hooks"].as_object_mut() {
        for event in GROK_HOOK_EVENTS {
            hooks.insert(
                event.to_string(),
                serde_json::json!([build_grok_hook_entry()]),
            );
        }
    }
    let grok_str = serde_json::to_string_pretty(&grok_config).map_err(|e| e.to_string())?;

    // agy 配置片段 — 只有我们那一组,合并进 hooks.json 顶层即可
    let mut agy_config = serde_json::Map::new();
    agy_config.insert(
        AGY_HOOK_GROUP.to_string(),
        Value::Object(build_agy_group(&hook_path)),
    );
    let agy_str =
        serde_json::to_string_pretty(&Value::Object(agy_config)).map_err(|e| e.to_string())?;

    Ok(serde_json::json!({
        "claude": {
            "file": "~/.claude/settings.json",
            "content": claude_str
        },
        "grok": {
            "files": [
                {
                    "file": format!("~/.grok/hooks/{}", GROK_HOOK_FILE),
                    "content": grok_str
                },
                {
                    // 命令是相对 hook JSON 的文件名：必须把二进制放到同一目录，
                    // 否则 grok 直接 spawn 时找不到（一键注册会自动复制）
                    "file": format!("~/.grok/hooks/{}", hook_binary_name()),
                    "note": "复制自",
                    "content": hook_path.clone()
                }
            ]
        },
        "codex": {
            "files": [
                {
                    "file": "~/.codex/hooks.json",
                    "content": codex_str
                },
                {
                    "file": "~/.codex/config.toml",
                    "note": "追加以下内容",
                    "content": "[features]\nhooks = true"
                }
            ]
        },
        "omp": {
            "files": [
                {
                    // 扩展文件不含机器相关信息，整份照抄即可；omp 只在启动时扫描
                    // 扩展目录，保存后要重启或 /reload-plugins
                    "file": ts_snippet_file(HookAgent::Omp, "~/.omp/agent"),
                    "note": "整份保存；omp 需重启或执行 /reload-plugins",
                    "content": OMP_EXTENSION_SOURCE
                }
            ]
        },
        "pi": {
            "files": [
                {
                    // 与 omp 同理;pi 的模板另一份,两家不能互抄
                    "file": ts_snippet_file(HookAgent::Pi, "~/.pi/agent"),
                    "note": "整份保存；pi 需重启或执行 /reload",
                    "content": PI_EXTENSION_SOURCE
                }
            ]
        },
        "agy": {
            "files": [
                {
                    // 与别家的组并列放在顶层;千万别加 PreToolUse(见 AGY_HOOK_EVENTS)
                    "file": "~/.gemini/config/hooks.json",
                    "note": format!("合并进顶层对象（组名 {}），别家的组原样保留", AGY_HOOK_GROUP),
                    "content": agy_str
                }
            ]
        }
    }))
}

#[cfg(test)]
mod tests;
