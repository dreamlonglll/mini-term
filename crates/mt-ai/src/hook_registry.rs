//! Hook 注册/卸载模块
//!
//! 一键把 Claude Code / Codex / Grok / oh-my-pi / pi 各家的 hook 配置写进各自的配置文件，
//! 并提供配置片段供用户手动粘贴。原本是一组 Tauri command，迁移后是普通函数。
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

// ─── Claude Code hook 注册/卸载 ───

/// 为 Claude Code 构建单个 hook 条目
///
/// Claude Code 格式要求: { "matcher": "", "hooks": [{ "type": "command", "command": "..." }] }
fn build_claude_hook_entry(hook_path: &str, event: &str) -> Value {
    let command = if cfg!(windows) {
        format!("\"{}\" {}", hook_path, event)
    } else {
        format!("{} {}", hook_path, event)
    };
    serde_json::json!({
        "matcher": "",
        "hooks": [{
            "type": "command",
            "command": command
        }]
    })
}

/// 注册 Claude Code hooks 到 ~/.claude/settings.json
fn register_claude_hooks(hook_path: &str) -> Result<String, String> {
    let settings_path =
        claude_settings::settings_path().ok_or_else(|| "无法获取 home 目录".to_string())?;

    // 读改写走共用助手:建 .claude 目录 / 读现有配置 / 原子写回 / 与 ssh_registry
    // 进程内串行,口径见 `claude_settings` 模块注释
    let (added, updated) = claude_settings::update(&settings_path, |settings| {
        // 确保 hooks 对象存在
        if settings.get("hooks").is_none() {
            settings["hooks"] = serde_json::json!({});
        }

        let hooks = settings["hooks"]
            .as_object_mut()
            .ok_or_else(|| "hooks 字段不是对象".to_string())?;

        let mut updated = 0;
        let mut added = 0;

        for event in CLAUDE_HOOK_EVENTS {
            let new_entry = build_claude_hook_entry(hook_path, event);

            if let Some(event_hooks) = hooks.get_mut(*event) {
                if let Some(arr) = event_hooks.as_array_mut() {
                    // 查找已有的 miniterm-hook 条目
                    // Claude Code 格式: [{ "matcher": "", "hooks": [{ "command": "..." }] }]
                    let existing_idx = arr.iter().position(entry_is_miniterm);

                    if let Some(idx) = existing_idx {
                        arr[idx] = new_entry;
                        updated += 1;
                    } else {
                        arr.push(new_entry);
                        added += 1;
                    }
                }
            } else {
                hooks.insert(event.to_string(), serde_json::json!([new_entry]));
                added += 1;
            }
        }
        Ok((added, updated))
    })?;

    Ok(format!(
        "Claude Code: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        CLAUDE_HOOK_EVENTS.len()
    ))
}

/// 从同构的 `{ "hooks": { "<Event>": [entry, …] } }` 里摘掉全部 mini-term 条目,
/// 返回摘掉的条数。Claude / Codex / Grok 三家的文件在这一层同构,共用本函数。
///
/// 扫 `hooks` 下**每个**事件键,不只当前事件表:老版本注册过、后来从事件表里下线的
/// 事件也得摘干净(卸载后不该留下任何指向 miniterm-hook 的条目)。只删被**我们**摘空
/// 的事件键 —— 用户自己留着的空数组不碰;删键用 `retain` 保住其余键的顺序
/// (`Map::remove` 在 preserve_order 下是 swap_remove,会打乱用户文件的键序)。
fn strip_miniterm_entries(config: &mut Value) -> usize {
    let Some(hooks) = config.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return 0;
    };
    let mut removed = 0;
    let mut emptied: Vec<String> = Vec::new();
    for (event, entries) in hooks.iter_mut() {
        let Some(arr) = entries.as_array_mut() else {
            continue;
        };
        let before = arr.len();
        arr.retain(|entry| !entry_is_miniterm(entry));
        let n = before - arr.len();
        if n > 0 && arr.is_empty() {
            emptied.push(event.clone());
        }
        removed += n;
    }
    if !emptied.is_empty() {
        hooks.retain(|event, _| !emptied.contains(event));
    }
    removed
}

/// 摘掉 Claude settings.json 里的 mini-term 条目,返回摘掉的条数。
///
/// 文件不在 / 读不懂 / 里面没有我们的条目时**一个字节不写**(返回 0):settings.json 是
/// 用户的全局配置,读改写一遍哪怕内容不变也会重排格式。
fn remove_claude_entries_at(path: &std::path::Path) -> Result<usize, String> {
    if registered_events_in(Some(path.to_path_buf())).is_empty() {
        return Ok(0);
    }
    claude_settings::update(path, |settings| Ok(strip_miniterm_entries(settings)))
}

/// 从 ~/.claude/settings.json 中卸载 miniterm hooks
fn unregister_claude_hooks() -> Result<String, String> {
    let removed = match claude_settings::settings_path() {
        Some(path) => remove_claude_entries_at(&path)?,
        None => 0,
    };
    Ok(if removed == 0 {
        "Claude Code: 未发现 mini-term 的 hook 条目，无需卸载".to_string()
    } else {
        format!("Claude Code: 已移除 {} 个 hook 条目", removed)
    })
}

/// 某个 hook 配置文件里已写入 miniterm-hook 条目的事件名集合。
///
/// 三家的文件在这一层同构（`{ "hooks": { "<Event>": [entry, …] } }`），共用本函数。
/// 读不到 / 解析失败一律返回空集 —— 空集的语义是「没注册过」，调用方据此不动手，
/// 比冒险改写一个读不懂的配置文件安全。
fn registered_events_in(path: Option<PathBuf>) -> std::collections::HashSet<String> {
    let mut set = std::collections::HashSet::new();
    let Some(path) = path else {
        return set;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return set;
    };
    let Ok(config) = serde_json::from_str::<Value>(&content) else {
        return set;
    };
    let Some(hooks) = config.get("hooks").and_then(|h| h.as_object()) else {
        return set;
    };
    for (event, entries) in hooks {
        let ours = entries
            .as_array()
            .is_some_and(|arr| arr.iter().any(entry_is_miniterm));
        if ours {
            set.insert(event.clone());
        }
    }
    set
}

fn registered_claude_events() -> std::collections::HashSet<String> {
    registered_events_in(claude_settings::settings_path())
}

fn registered_codex_events() -> std::collections::HashSet<String> {
    registered_events_in(codex_hooks_path())
}

/// 需要补注册的事件（`sync_claude_hooks_if_registered` 的纯判定部分，抽出来是
/// 为了可测：另一半要读用户 home 目录下的真实配置）。
///
/// `registered` 为空 = 从未注册过，返回空 —— 不给没开过这功能的用户写配置。
fn missing_claude_events(registered: &std::collections::HashSet<String>) -> Vec<&'static str> {
    if registered.is_empty() {
        return Vec::new();
    }
    CLAUDE_HOOK_EVENTS
        .iter()
        .copied()
        .filter(|e| !registered.contains(*e))
        .collect()
}

/// 给已注册的用户补上新版本新增的 hook 事件。
///
/// `CLAUDE_HOOK_EVENTS` 会随版本增长（v0.10.3 补了 StopFailure 等 5 个），而注册
/// 是设置面板里的一次性手动动作。不补的话，老用户升级后配置里永远是旧事件集，
/// 新增的状态判定对他们完全不生效——而他们没有任何理由知道要再点一次「注册」。
///
/// 只在**已经注册过**（配置里存在 miniterm-hook 条目）时补：从未注册的用户不碰，
/// 那是他们的选择。补齐直接复用幂等的 `register_claude_hooks`。
pub fn sync_claude_hooks_if_registered() {
    let missing = missing_claude_events(&registered_claude_events());
    if missing.is_empty() {
        return;
    }
    let hook_path = match hook_binary_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[hook-registry] 补注册跳过（拿不到 hook 二进制路径）: {}", e);
            return;
        }
    };
    match register_claude_hooks(&hook_path) {
        Ok(msg) => eprintln!("[hook-registry] 补注册新增事件 {:?} -> {}", missing, msg),
        Err(e) => eprintln!("[hook-registry] 补注册失败: {}", e),
    }
}

// ─── Codex hook 注册/卸载 ───

/// 获取 Codex 事件的超时时间
fn codex_event_timeout(event: &str) -> u64 {
    if event == "PermissionRequest" {
        600
    } else {
        30
    }
}

/// 为 Codex 构建单个 hook 条目
///
/// Codex 在 Windows 上使用 PowerShell 执行 hook 命令，
/// 需要用 call operator (`& "path"`) 格式。
fn build_codex_hook_entry(hook_path: &str, event: &str) -> Value {
    let command = if cfg!(windows) {
        format!("& \"{}\" {}", hook_path, event)
    } else {
        format!("{} {}", hook_path, event)
    };
    serde_json::json!([{
        "hooks": [{
            "type": "command",
            "command": command,
            "timeout": codex_event_timeout(event)
        }]
    }])
}

/// 将 Codex config.toml 更新为当前 hooks feature，并迁移旧版键。
///
/// 抽成纯函数，避免测试改写开发者真实的 `~/.codex/config.toml`。
fn enable_codex_hooks_feature(content: &str) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("解析 config.toml 失败: {}", e))?;

    if doc.get("features").is_none() {
        doc["features"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let features = doc["features"]
        .as_table_like_mut()
        .ok_or_else(|| "config.toml 的 features 字段不是表".to_string())?;

    // Codex CLI 0.152.1 起改用 `hooks`；移除旧键，避免新版本报告未知 feature。
    features.remove("codex_hooks");
    features.insert("hooks", toml_edit::value(true));
    Ok(doc.to_string())
}

/// 确保 Codex config.toml 中启用了当前 hooks feature flag。
///
/// 返回是否真的落盘：键已经是现行名字时原样返回，一个字节都不写。启动期自愈
/// （`sync_codex_hooks_feature_if_registered`）走的就是这条路，不能每次启动都
/// 重写一遍用户的 config.toml。
fn ensure_codex_hooks_feature() -> Result<bool, String> {
    let config_path = codex_config_path().ok_or_else(|| "无法获取 home 目录".to_string())?;

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 .codex 目录失败: {}", e))?;
    }

    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .map_err(|e| format!("读取 config.toml 失败: {}", e))?
    } else {
        String::new()
    };
    let updated = enable_codex_hooks_feature(&content)?;
    if updated == content {
        return Ok(false);
    }

    crate::util::atomic_write(&config_path, updated.as_bytes())
        .map_err(|e| format!("写入 config.toml 失败: {}", e))?;

    Ok(true)
}

/// 已注册用户的启动期自愈：把 config.toml 里的 feature 键迁到当前名字。
///
/// config.toml 只有点「注册」时才写，而面板判「已注册」只看 hooks.json——存量
/// 用户升级 mini-term 后面板照旧显示已注册，config.toml 里却还留着废弃的
/// `codex_hooks`，每次开 codex 都吃一条弃用告警（issue #72），且界面上没有任何
/// 线索提示他该去重点一次注册。故只要 hooks.json 里有我们的条目就迁一次。
pub fn sync_codex_hooks_feature_if_registered() {
    if registered_codex_events().is_empty() {
        return;
    }
    match ensure_codex_hooks_feature() {
        Ok(true) => eprintln!("[hook-registry] codex config.toml 已迁至 features.hooks"),
        Ok(false) => {}
        Err(e) => eprintln!("[hook-registry] codex feature 迁移失败: {}", e),
    }
}

/// 注册 Codex hooks 到 ~/.codex/hooks.json
fn register_codex_hooks(hook_path: &str) -> Result<String, String> {
    let hooks_path = codex_hooks_path().ok_or_else(|| "无法获取 home 目录".to_string())?;

    // 确保 .codex 目录存在
    if let Some(parent) = hooks_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 .codex 目录失败: {}", e))?;
    }

    // 启用 feature flag
    ensure_codex_hooks_feature()?;

    // 读取现有配置
    let mut config: Value = if hooks_path.exists() {
        let content = std::fs::read_to_string(&hooks_path)
            .map_err(|e| format!("读取 hooks.json 失败: {}", e))?;
        serde_json::from_str(&content).map_err(|e| format!("解析 hooks.json 失败: {}", e))?
    } else {
        serde_json::json!({})
    };

    // 确保 hooks 对象存在
    if config.get("hooks").is_none() {
        config["hooks"] = serde_json::json!({});
    }

    let hooks = config["hooks"]
        .as_object_mut()
        .ok_or_else(|| "hooks 字段不是对象".to_string())?;

    let mut updated = 0;
    let mut added = 0;

    for event in CODEX_HOOK_EVENTS {
        let new_entries = build_codex_hook_entry(hook_path, event);

        if let Some(event_hooks) = hooks.get_mut(*event) {
            if let Some(arr) = event_hooks.as_array_mut() {
                // 查找已有的 miniterm-hook 条目
                // Codex 格式: [ { "hooks": [{ "type": "command", "command": "..." }] } ]
                let existing_idx = arr.iter().position(entry_is_miniterm);

                if let Some(idx) = existing_idx {
                    // 更新：替换整个条目
                    if let Some(new_entry) = new_entries.as_array().and_then(|a| a.first()) {
                        arr[idx] = new_entry.clone();
                        updated += 1;
                    }
                } else {
                    // 追加
                    if let Some(new_arr) = new_entries.as_array() {
                        for entry in new_arr {
                            arr.push(entry.clone());
                        }
                    }
                    added += 1;
                }
            }
        } else {
            // 创建新的事件条目
            hooks.insert(event.to_string(), new_entries);
            added += 1;
        }
    }

    // 写回配置文件
    let json_str = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("序列化 hooks.json 失败: {}", e))?;
    crate::util::atomic_write(&hooks_path, json_str.as_bytes())
        .map_err(|e| format!("写入 hooks.json 失败: {}", e))?;

    Ok(format!(
        "Codex: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        CODEX_HOOK_EVENTS.len()
    ))
}

/// 摘掉 Codex hooks.json 里的 mini-term 条目,返回摘掉的条数。没有我们的条目时
/// 一个字节不写(同 [`remove_claude_entries_at`])。
///
/// `config.toml` 里注册时打开的 `features.hooks` 不动:那是 Codex 的总开关,用户自己
/// 的 hook 也靠它,开着而没有条目是无害的。
fn remove_codex_entries_at(path: &std::path::Path) -> Result<usize, String> {
    if registered_events_in(Some(path.to_path_buf())).is_empty() {
        return Ok(0);
    }
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("读取 hooks.json 失败: {}", e))?;
    let mut config: Value =
        serde_json::from_str(&content).map_err(|e| format!("解析 hooks.json 失败: {}", e))?;
    let removed = strip_miniterm_entries(&mut config);

    let json_str = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("序列化 hooks.json 失败: {}", e))?;
    crate::util::atomic_write(path, json_str.as_bytes())
        .map_err(|e| format!("写入 hooks.json 失败: {}", e))?;
    Ok(removed)
}

/// 从 ~/.codex/hooks.json 中卸载 miniterm hooks
fn unregister_codex_hooks() -> Result<String, String> {
    let removed = match codex_hooks_path() {
        Some(path) => remove_codex_entries_at(&path)?,
        None => 0,
    };
    Ok(if removed == 0 {
        "Codex: 未发现 mini-term 的 hook 条目，无需卸载".to_string()
    } else {
        format!("Codex: 已移除 {} 个 hook 条目", removed)
    })
}

// ─── Grok Build hook 注册/卸载 ───

/// 为 Grok 构建单个 hook 条目。
///
/// 与另外两家最大的不同：**命令是不带参数的相对文件名**。grok 的 runner 只在
/// 命令文本含空格/管道/`&`/`$` 等元字符时才交给 shell，而 Windows 上它挑的 shell
/// 由环境决定（git-bash / pwsh / powershell / cmd 依次探测），四家的引号与调用
/// 语义互斥——`"C:\path\x.exe" Event` 在 PowerShell 里只是个字符串字面量，
/// `& "…"` 在 bash/cmd 里又是语法错误，写不出一份通用文本。
/// 不含空格的相对路径（相对 hook JSON 所在目录）走的是直接 spawn 分支，
/// 完全绕开 shell；事件名改由 grok 注入的 `GROK_HOOK_EVENT` 传递
/// （sidecar 的 `resolve_event_name` 负责 snake_case → PascalCase 还原）。
///
/// 不写 `matcher`：grok 对 `Stop` / `UserPromptSubmit` 上的 matcher 会打警告，
/// 而空 matcher 本就等价于「匹配全部」，省掉即可。
fn build_grok_hook_entry() -> Value {
    serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": hook_binary_name(),
            "timeout": GROK_HOOK_TIMEOUT_SECS
        }]
    })
}

/// 把 hook 二进制复制进 grok hooks 目录。
///
/// 复制失败但旧副本还在 → 视为成功（Windows 上覆盖正在运行的 exe 会失败，
/// 而 hook 进程虽短命也可能恰好在跑；此时留着旧副本远好过让整次注册失败）。
fn install_grok_hook_binary(src: &str) -> Result<(), String> {
    let dest = grok_hook_binary_path().ok_or_else(|| "无法获取 grok 目录".to_string())?;
    let src_path = std::path::Path::new(src);
    if !src_path.is_file() {
        return Err(format!("hook 二进制不存在: {}", src));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 hooks 目录失败: {}", e))?;
    }
    match std::fs::copy(src_path, &dest) {
        Ok(_) => Ok(()),
        Err(e) if dest.is_file() => {
            eprintln!("[hook-registry] grok hook 二进制覆盖失败(沿用旧副本): {}", e);
            Ok(())
        }
        Err(e) => Err(format!("复制 hook 二进制失败: {}", e)),
    }
}

/// 注册 Grok hooks 到 {grok_home}/hooks/miniterm.json
fn register_grok_hooks(hook_path: &str) -> Result<String, String> {
    let hooks_path = grok_hooks_path().ok_or_else(|| "无法获取 grok 目录".to_string())?;

    install_grok_hook_binary(hook_path)?;

    let mut config: Value = if hooks_path.exists() {
        let content = std::fs::read_to_string(&hooks_path)
            .map_err(|e| format!("读取 {} 失败: {}", GROK_HOOK_FILE, e))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("解析 {} 失败: {}", GROK_HOOK_FILE, e))?
    } else {
        serde_json::json!({})
    };

    if config.get("hooks").is_none() {
        config["hooks"] = serde_json::json!({});
    }
    let hooks = config["hooks"]
        .as_object_mut()
        .ok_or_else(|| "hooks 字段不是对象".to_string())?;

    let mut updated = 0;
    let mut added = 0;

    for event in GROK_HOOK_EVENTS {
        let new_entry = build_grok_hook_entry();
        if let Some(arr) = hooks.get_mut(*event).and_then(|v| v.as_array_mut()) {
            match arr.iter().position(entry_is_miniterm) {
                Some(idx) => {
                    arr[idx] = new_entry;
                    updated += 1;
                }
                None => {
                    arr.push(new_entry);
                    added += 1;
                }
            }
        } else {
            hooks.insert(event.to_string(), serde_json::json!([new_entry]));
            added += 1;
        }
    }

    let json_str = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("序列化 {} 失败: {}", GROK_HOOK_FILE, e))?;
    crate::util::atomic_write(&hooks_path, json_str.as_bytes())
        .map_err(|e| format!("写入 {} 失败: {}", GROK_HOOK_FILE, e))?;

    Ok(format!(
        "Grok: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        GROK_HOOK_EVENTS.len()
    ))
}

/// 摘掉 {grok_home}/hooks/miniterm.json 里的 mini-term 条目,返回摘掉的条数;
/// 条目清空后连同复制进去的二进制(`bin`)一并删除(那份副本只为本文件服务)。
/// 没有我们的条目时一个字节不动(同 [`remove_claude_entries_at`])。
fn remove_grok_entries_at(
    hooks_path: &std::path::Path,
    bin: Option<&std::path::Path>,
) -> Result<usize, String> {
    if registered_events_in(Some(hooks_path.to_path_buf())).is_empty() {
        return Ok(0);
    }
    let content = std::fs::read_to_string(hooks_path)
        .map_err(|e| format!("读取 {} 失败: {}", GROK_HOOK_FILE, e))?;
    let mut config: Value = serde_json::from_str(&content)
        .map_err(|e| format!("解析 {} 失败: {}", GROK_HOOK_FILE, e))?;
    let removed = strip_miniterm_entries(&mut config);

    let file_now_empty = config
        .get("hooks")
        .and_then(|h| h.as_object())
        .is_none_or(|h| h.is_empty());

    if file_now_empty {
        // 整个文件都是我们的：直接删掉，别在用户的 hooks 目录留下空壳
        // （sidecar 按该文件是否存在决定要不要丢弃 Claude 兼容层的重复投递）
        std::fs::remove_file(hooks_path)
            .map_err(|e| format!("删除 {} 失败: {}", GROK_HOOK_FILE, e))?;
        if let Some(bin) = bin.filter(|b| b.is_file())
            && let Err(e) = std::fs::remove_file(bin)
        {
            eprintln!("[hook-registry] 删除 grok hook 二进制副本失败: {}", e);
        }
    } else {
        let json_str = serde_json::to_string_pretty(&config)
            .map_err(|e| format!("序列化 {} 失败: {}", GROK_HOOK_FILE, e))?;
        crate::util::atomic_write(hooks_path, json_str.as_bytes())
            .map_err(|e| format!("写入 {} 失败: {}", GROK_HOOK_FILE, e))?;
    }
    Ok(removed)
}

/// 从 {grok_home}/hooks/miniterm.json 中卸载 miniterm hooks
fn unregister_grok_hooks() -> Result<String, String> {
    let removed = match grok_hooks_path() {
        Some(path) => remove_grok_entries_at(&path, grok_hook_binary_path().as_deref())?,
        None => 0,
    };
    Ok(if removed == 0 {
        "Grok: 未发现 mini-term 的 hook 条目，无需卸载".to_string()
    } else {
        format!("Grok: 已移除 {} 个 hook 条目", removed)
    })
}

fn registered_grok_events() -> std::collections::HashSet<String> {
    registered_events_in(grok_hooks_path())
}

fn missing_grok_events(registered: &std::collections::HashSet<String>) -> Vec<&'static str> {
    if registered.is_empty() {
        return Vec::new();
    }
    GROK_HOOK_EVENTS
        .iter()
        .copied()
        .filter(|e| !registered.contains(*e))
        .collect()
}

/// 已注册用户的启动期自愈，两件事：补齐新增事件，以及**刷新二进制副本**。
///
/// 副本是 grok 路线特有的负担：mini-term 升级后应用目录里的 hook 二进制换了新的，
/// 而 `~/.grok/hooks/` 里那份还是旧的。只要注册过就无条件重跑一次幂等的注册，
/// 顺带把副本盖成当前版本（覆盖失败会沿用旧副本，不会让启动流程报错）。
pub fn sync_grok_hooks_if_registered() {
    let registered = registered_grok_events();
    if registered.is_empty() {
        return;
    }
    let hook_path = match hook_binary_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[hook-registry] grok 补注册跳过（拿不到 hook 二进制路径）: {}", e);
            return;
        }
    };
    let missing = missing_grok_events(&registered);
    match register_grok_hooks(&hook_path) {
        Ok(msg) => eprintln!(
            "[hook-registry] grok 补注册(缺失事件 {:?}) -> {}",
            missing, msg
        ),
        Err(e) => eprintln!("[hook-registry] grok 补注册失败: {}", e),
    }
}

// ─── 进程内 TS 扩展（oh-my-pi / pi）注册/卸载 ───
//
// 两家同一套做法（[`TsExtension`]）：整份模板写进 `{agent_dir}/extensions/miniterm.ts`。
// 「注册现状」的判据是扩展文件在场且带 `miniterm-hook` 标识；事件计数按文件里
// `pi.on("<event>"` 的出现与否对账，老版本模板缺的事件就显示成「旧版本 N/M」。
// 启动期自愈（`sync_*_hooks_if_registered`）在文件内容与当前模板不同时整份重写
// ——模板修了 bug / 加了事件，老用户下次启动即拿到，不必再点一次「注册」。
//
// 两家的扩展 API 已经分叉（pi 有 `agent_settled`、换会话走 shutdown + start；omp
// 有 `session_switch` / 工具审批事件），模板各写一份、不能互换。文件名相同是有意的：
// 有人把 omp 的模板手抄进 pi 的扩展目录（issue #85 就是这么试的），它带着标识，
// pi 的自愈会认出「旧版本」并换成 pi 的模板。设了 `PI_CODING_AGENT_DIR` 时两家落在
// 同一个文件上，归属改按模板自报的 `const AGENT` 认（见 [`TsExtension::owns`]）。

/// 写进扩展目录的文件名（omp 与 pi 相同，各在各的 agent 目录下）
const EXTENSION_FILE: &str = "miniterm.ts";

/// omp 扩展源码（随主程序编译进来，注册时整份落盘）。文件本身不含任何机器相关
/// 信息：端口走 `MINITERM_HOOK_PORT` / hook-server.json，pane 走 `MINITERM_PTY_ID`，
/// 所以任何机器上的内容都逐字相同，「是否需要刷新」按整份相等判断即可。
const OMP_EXTENSION_SOURCE: &str = include_str!("../assets/miniterm-omp.ts");

/// omp 扩展订阅的 omp 事件（omp 自己的 snake_case 事件名）。面板的「已注册 N/M」
/// 与启动期补齐按它对账；与 `OMP_EXTENSION_SOURCE` 里的 `pi.on(...)` 逐条对应
/// （单测钉住两边一致）。
const OMP_HOOK_EVENTS: &[&str] = &[
    "session_start",
    "session_switch",
    "session_branch",
    "session_shutdown",
    "agent_start",
    "agent_end",
    "tool_call",
    "tool_result",
    "tool_approval_requested",
    "tool_approval_resolved",
    "auto_retry_start",
    "auto_compaction_start",
    "auto_compaction_end",
];

/// omp 扩展会上报给 hook 服务器的事件名（PascalCase，与 Claude 同名）。
/// `hook_server::map_event_to_status` 必须认识其中每一个（`SessionEnd` 单独处理），
/// 由 hook_server 的单测钉住——漏一个就是一段状态空洞。
/// （`pub` 而非 `pub(crate)`：它只被单测消费，crate 私有会在非测试构建里报 dead_code。）
pub const OMP_REPORTED_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Elicitation",
    "ElicitationResult",
    "PermissionRequest",
    "PermissionDenied",
    "Notification",
    "PreCompact",
    "PostCompact",
];

/// omp 的 agent 目录（纯函数，环境取值由 [`omp_agent_dir`] 负责）：
/// `PI_CODING_AGENT_DIR` 优先；否则 `~/{PI_CONFIG_DIR 或 .omp}/agent`，带 `PI_PROFILE`
/// 时是 `~/.omp/profiles/{profile}/agent`——与 omp 自身 `getAgentDir()` 的口径一致。
/// Linux 上经 `omp config init-xdg` 迁走的 XDG 布局不支持：那是罕见的显式迁移。
fn omp_agent_dir_from(
    home: &std::path::Path,
    coding_agent_dir: Option<&str>,
    config_dir: Option<&str>,
    profile: Option<&str>,
) -> PathBuf {
    if let Some(dir) = coding_agent_dir.filter(|d| !d.trim().is_empty()) {
        return PathBuf::from(dir);
    }
    let config_dir = config_dir
        .filter(|d| !d.trim().is_empty())
        .unwrap_or(".omp");
    let root = home.join(config_dir);
    let root = match profile.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => root.join("profiles").join(p),
        None => root,
    };
    root.join("agent")
}

/// omp 的用户级 agent 目录（缺省 `~/.omp/agent`）
pub fn omp_agent_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let env = |k: &str| std::env::var(k).ok();
    Some(omp_agent_dir_from(
        &home,
        env("PI_CODING_AGENT_DIR").as_deref(),
        env("PI_CONFIG_DIR").as_deref(),
        env("PI_PROFILE").as_deref(),
    ))
}

/// pi 扩展源码（同 [`OMP_EXTENSION_SOURCE`]，不含任何机器相关信息）
const PI_EXTENSION_SOURCE: &str = include_str!("../assets/miniterm-pi.ts");

/// pi 扩展订阅的 pi 事件，与 `PI_EXTENSION_SOURCE` 里的 `pi.on(...)` 逐条对应
/// （单测钉住两边一致）。`agent_settled` 需 pi ≥ 0.80.4、`ui_prompt_*` 需 ≥ 0.84.4；
/// 更早的 pi 收不到这几个事件，回合结束靠停摆兜底收敛（不播报完成）。
const PI_HOOK_EVENTS: &[&str] = &[
    "session_start",
    "session_shutdown",
    "agent_start",
    "agent_end",
    "agent_settled",
    "tool_call",
    "tool_result",
    "ui_prompt_start",
    "ui_prompt_end",
];

/// pi 扩展会上报给 hook 服务器的事件名，口径同 [`OMP_REPORTED_EVENTS`]。
pub const PI_REPORTED_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "Stop",
    "StopFailure",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "Elicitation",
    "ElicitationResult",
];

/// pi 的 agent 目录（纯函数，环境取值由 [`pi_agent_dir`] 负责）：`PI_CODING_AGENT_DIR`
/// 优先（与 pi 的 `normalizePath` 一样认 `~` / `~/` 前缀，Windows 上另认 `~\`），
/// 否则 `~/.pi/agent`——与 pi 自身 `getAgentDir()` 的口径一致。
///
/// ⚠️ omp 也认 `PI_CODING_AGENT_DIR`（见 [`omp_agent_dir_from`]）。设了它，两家落在
/// 同一个目录、只放得下一个 `miniterm.ts`：归属按模板里的 `const AGENT = "…"` 认
/// （[`TsExtension::owns`]），自愈不再把对方的文件当成自己的旧版改写。
fn pi_agent_dir_from(home: &std::path::Path, coding_agent_dir: Option<&str>) -> PathBuf {
    let Some(dir) = coding_agent_dir.filter(|d| !d.trim().is_empty()) else {
        return home.join(".pi").join("agent");
    };
    if dir == "~" {
        return home.to_path_buf();
    }
    let rest = dir
        .strip_prefix("~/")
        .or_else(|| cfg!(windows).then(|| dir.strip_prefix("~\\")).flatten());
    match rest {
        Some(rest) => home.join(rest),
        None => PathBuf::from(dir),
    }
}

/// pi 的用户级 agent 目录（缺省 `~/.pi/agent`）
pub fn pi_agent_dir() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(pi_agent_dir_from(
        &home,
        std::env::var("PI_CODING_AGENT_DIR").ok().as_deref(),
    ))
}

/// 一家「hook 是进程内 TS 扩展」的 CLI：整份模板写进 `{agent_dir}/extensions/miniterm.ts`。
struct TsExtension {
    /// 结果文案里的展示名（与 agent 表的 label 一致）
    label: &'static str,
    /// 日志里的短名
    tag: &'static str,
    source: &'static str,
    /// 模板订阅的事件（面板「N/M」的分母）
    events: &'static [&'static str],
    agent_dir: fn() -> Option<PathBuf>,
    /// 正在运行的实例怎么装上新扩展：它们都只在启动时扫描扩展目录，结果文案里
    /// 必须把这句带给用户，否则他会以为注册没生效。
    reload_hint: &'static str,
}

const OMP_EXTENSION: TsExtension = TsExtension {
    label: "oh-my-pi",
    tag: "omp",
    source: OMP_EXTENSION_SOURCE,
    events: OMP_HOOK_EVENTS,
    agent_dir: omp_agent_dir,
    reload_hint: "正在运行的 omp 需重启或执行 /reload-plugins 才生效",
};

const PI_EXTENSION: TsExtension = TsExtension {
    label: "pi",
    tag: "pi",
    source: PI_EXTENSION_SOURCE,
    events: PI_HOOK_EVENTS,
    agent_dir: pi_agent_dir,
    reload_hint: "正在运行的 pi 需重启或执行 /reload 才生效",
};

/// 模板自报的身份(`const AGENT = "omp";` 里的那个串);没有这一行返回 `None`。
fn template_agent(source: &str) -> Option<&str> {
    let rest = &source[source.find("const AGENT = \"")? + "const AGENT = \"".len()..];
    rest.find('"').map(|end| &rest[..end])
}

impl TsExtension {
    /// mini-term 写入的扩展文件路径：`{agent_dir}/extensions/miniterm.ts`
    fn path(&self) -> Option<PathBuf> {
        (self.agent_dir)().map(|d| d.join("extensions").join(EXTENSION_FILE))
    }

    /// 同一类的另一家(omp ↔ pi)
    fn other(&self) -> &'static TsExtension {
        if self.tag == OMP_EXTENSION.tag {
            &PI_EXTENSION
        } else {
            &OMP_EXTENSION
        }
    }

    /// 两家是否落在同一个文件上(设了 `PI_CODING_AGENT_DIR`,见 [`pi_agent_dir_from`])
    fn shares_file_with_other(&self) -> bool {
        self.path().is_some_and(|p| Some(p) == self.other().path())
    }

    /// 这份文件算不算本家的(含旧版)。不带 miniterm-hook 标识的不是我们写的,一律不算
    /// ——绝不把用户自己的同名扩展算成我们的。带标识的按模板自报的身份认:
    ///
    /// - 是本家 → 算;
    /// - 是另一家:两家共用一个文件时,那就是另一家的正经注册,**不能**算(否则启动
    ///   自愈会把它改写成本家模板——只用 omp 的用户每次启动都被换成 pi 模板,完成
    ///   判定随之失效);各在各的目录时,只可能是手抄错了地方(issue #85 就是把 omp
    ///   的模板抄进了 pi 的目录),算成本家旧版,自愈好把它换掉;
    /// - 没有身份行(不是任何一版模板写出来的)→ 按所在目录认。
    fn owns(&self, source: &str, shared_file: bool) -> bool {
        source.contains(HOOK_MARKER)
            && match template_agent(source) {
                Some(agent) => agent == self.tag || !shared_file,
                None => true,
            }
    }

    /// 某段扩展源码里订阅了本家的哪些事件（`pi.on("<event>"`）;不是本家的文件
    /// ([`Self::owns`])一律按「没注册」。
    fn events_in_source(
        &self,
        source: &str,
        shared_file: bool,
    ) -> std::collections::HashSet<String> {
        if !self.owns(source, shared_file) {
            return std::collections::HashSet::new();
        }
        self.events
            .iter()
            .filter(|e| source.contains(&format!("pi.on(\"{}\"", e)))
            .map(|e| e.to_string())
            .collect()
    }

    fn registered_events(&self) -> std::collections::HashSet<String> {
        let Some(path) = self.path() else {
            return std::collections::HashSet::new();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return std::collections::HashSet::new();
        };
        self.events_in_source(&content, self.shares_file_with_other())
    }

    /// 把扩展写到位。返回是否真的落盘：内容已是当前模板时一个字节不写（启动期自愈
    /// 每次启动都会走这里，不能每次都重写用户目录里的文件）。
    ///
    /// 同名文件在场却**不带**我们的标识 → 拒绝覆盖：那是用户自己的扩展，撞名也不能吃掉。
    fn write(&self) -> Result<bool, String> {
        let path = self
            .path()
            .ok_or_else(|| "无法获取 home 目录".to_string())?;
        let current = std::fs::read_to_string(&path).ok();
        if current.as_deref() == Some(self.source) {
            return Ok(false);
        }
        if current.is_some_and(|c| !c.contains(HOOK_MARKER)) {
            return Err(format!(
                "{} 已存在且不是 mini-term 写入的文件，未覆盖",
                path.display()
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("创建 extensions 目录失败: {}", e))?;
        }
        crate::util::atomic_write(&path, self.source.as_bytes())
            .map_err(|e| format!("写入 {} 失败: {}", EXTENSION_FILE, e))?;
        Ok(true)
    }

    fn register(&self) -> Result<String, String> {
        // 两家共用一个文件时,注册本家只能顶掉另一家:结果行里说清楚,别让人以为两家都装着
        let replaces_other = self.shares_file_with_other()
            && self
                .path()
                .and_then(|p| std::fs::read_to_string(p).ok())
                .is_some_and(|c| self.other().owns(&c, true));
        let written = self.write()?;
        Ok(if written {
            let replaced = if replaces_other {
                format!(
                    "；{} 与它共用 PI_CODING_AGENT_DIR 指向的目录，原来的 {} 扩展已被替换",
                    self.other().label,
                    self.other().label
                )
            } else {
                String::new()
            };
            format!(
                "{}: 扩展已写入 (共 {} 个事件)；{}{}",
                self.label,
                self.events.len(),
                self.reload_hint,
                replaced
            )
        } else {
            format!(
                "{}: 扩展已是最新 (共 {} 个事件)",
                self.label,
                self.events.len()
            )
        })
    }

    /// 卸载：整个文件都是我们的，直接删；不带标识的同名文件、以及两家共用一个文件时
    /// 另一家的注册([`Self::owns`])都不动。
    fn unregister(&self) -> Result<String, String> {
        let removed = self.remove_entries()?;
        Ok(if removed == 0 {
            format!(
                "{}: 未发现 mini-term 写入的 {}，无需卸载",
                self.label, EXTENSION_FILE
            )
        } else {
            format!("{}: 已移除扩展文件", self.label)
        })
    }

    fn remove_entries(&self) -> Result<usize, String> {
        let shared = self.shares_file_with_other();
        self.path().map_or(Ok(0), |path| {
            remove_extension_at(&path, |source| self.owns(source, shared))
        })
    }

    /// 已注册用户的启动期自愈：扩展文件与当前模板不同就整份重写。
    ///
    /// 与 grok 刷新二进制副本同一个理由——mini-term 升级后模板变了，用户目录里那份
    /// 还是旧的；只要注册过就无条件对齐，从未注册的用户不碰。
    fn sync_if_registered(&self) {
        if self.registered_events().is_empty() {
            return;
        }
        match self.write() {
            Ok(true) => eprintln!("[hook-registry] {} 扩展已刷新至当前模板", self.tag),
            Ok(false) => {}
            Err(e) => eprintln!("[hook-registry] {} 扩展刷新失败: {}", self.tag, e),
        }
    }
}

/// 删掉我们写的扩展文件,返回删掉的文件数(0 / 1)。文件不在、或 `owns` 不认它
/// (用户自己的扩展 / 共用目录里另一家的注册)时不动。
fn remove_extension_at(
    path: &std::path::Path,
    owns: impl Fn(&str) -> bool,
) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("读取 {} 失败: {}", EXTENSION_FILE, e))?;
    if !owns(&content) {
        return Ok(0);
    }
    std::fs::remove_file(path).map_err(|e| format!("删除 {} 失败: {}", EXTENSION_FILE, e))?;
    Ok(1)
}

/// omp 的启动期自愈，见 [`TsExtension::sync_if_registered`]
pub fn sync_omp_hooks_if_registered() {
    OMP_EXTENSION.sync_if_registered();
}

/// pi 的启动期自愈，见 [`TsExtension::sync_if_registered`]
pub fn sync_pi_hooks_if_registered() {
    PI_EXTENSION.sync_if_registered();
}

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
}

impl HookAgent {
    pub const ALL: &'static [HookAgent] = &[
        HookAgent::Claude,
        HookAgent::Codex,
        HookAgent::Grok,
        HookAgent::Omp,
        HookAgent::Pi,
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
        }
    }

    fn registered_events(self) -> std::collections::HashSet<String> {
        match self {
            HookAgent::Claude => registered_claude_events(),
            HookAgent::Codex => registered_codex_events(),
            HookAgent::Grok => registered_grok_events(),
            HookAgent::Omp => OMP_EXTENSION.registered_events(),
            HookAgent::Pi => PI_EXTENSION.registered_events(),
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
        }
    }

    fn unregister(self) -> Result<String, String> {
        match self {
            HookAgent::Claude => unregister_claude_hooks(),
            HookAgent::Codex => unregister_codex_hooks(),
            HookAgent::Grok => unregister_grok_hooks(),
            HookAgent::Omp => OMP_EXTENSION.unregister(),
            HookAgent::Pi => PI_EXTENSION.unregister(),
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
/// 五家配置里 mini-term 写入的条目全部摘掉,按家返回摘掉的条数。
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
            hooks.insert(event.to_string(), serde_json::json!([build_grok_hook_entry()]));
        }
    }
    let grok_str = serde_json::to_string_pretty(&grok_config).map_err(|e| e.to_string())?;

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
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 状态判定依赖的 Claude 事件必须都在注册列表里：注册列表是白名单，
    /// 漏一个就等于该时刻没有任何事件覆盖，状态只能卡到下一个事件。
    /// StopFailure 尤其关键——官方文档明确 API 错误结束回合时 `Stop` 不触发。
    #[test]
    fn claude_registration_covers_status_critical_events() {
        for event in [
            "SessionStart",
            "SessionEnd",
            "Stop",
            "StopFailure",
            "PostToolUse",
            "PostToolUseFailure",
            "PostToolBatch",
            "PermissionRequest",
            "PermissionDenied",
            "Elicitation",
            "ElicitationResult",
        ] {
            assert!(
                CLAUDE_HOOK_EVENTS.contains(&event),
                "{event} 未注册，该时刻的状态无事件覆盖"
            );
        }
    }

    /// 老用户（注册于事件列表增长之前）启动时应被补齐，且只补缺的那几个
    #[test]
    fn stale_registration_is_detected_as_missing() {
        // v0.10.2 及更早的事件集
        let old: std::collections::HashSet<String> = [
            "SessionStart",
            "SessionEnd",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "Stop",
            "SubagentStart",
            "SubagentStop",
            "PreCompact",
            "PostCompact",
            "PermissionRequest",
            "Notification",
            "Elicitation",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let missing = missing_claude_events(&old);
        assert!(missing.contains(&"StopFailure"), "StopFailure 未被识别为缺失");
        assert_eq!(missing.len(), CLAUDE_HOOK_EVENTS.len() - old.len());
    }

    /// 从未注册过的用户不该被静默写配置；已是最新的不该反复重写
    #[test]
    fn sync_is_noop_when_never_registered_or_already_current() {
        assert!(missing_claude_events(&std::collections::HashSet::new()).is_empty());

        let current: std::collections::HashSet<String> =
            CLAUDE_HOOK_EVENTS.iter().map(|s| s.to_string()).collect();
        assert!(missing_claude_events(&current).is_empty());
    }

    /// 事件名重复会在 settings.json 里写出两条相同 hook，AI 每次事件多跑一个进程
    #[test]
    fn claude_registration_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for event in CLAUDE_HOOK_EVENTS {
            assert!(seen.insert(*event), "重复注册事件: {event}");
        }
    }

    /// grok 的状态判定同样依赖这批事件；`Notification` 尤其关键——grok 没有
    /// `PermissionRequest`，「等待授权」只能从 Notification 的
    /// `permission_prompt` 类型认出来，漏注册就等于没有黄灯。
    #[test]
    fn grok_registration_covers_status_critical_events() {
        for event in [
            "SessionStart",
            "SessionEnd",
            "Stop",
            "StopFailure",
            "PostToolUse",
            "PostToolUseFailure",
            "PermissionDenied",
            "Notification",
        ] {
            assert!(
                GROK_HOOK_EVENTS.contains(&event),
                "{event} 未注册，该时刻的状态无事件覆盖"
            );
        }
    }

    /// grok 没有这些事件，注册了只会在 `/hooks` 面板里留下无效条目
    #[test]
    fn grok_registration_omits_events_grok_lacks() {
        for event in ["PermissionRequest", "PostToolBatch", "Elicitation", "ElicitationResult"] {
            assert!(!GROK_HOOK_EVENTS.contains(&event), "{event} 不是 grok 的事件");
        }
    }

    #[test]
    fn grok_registration_has_no_duplicates() {
        let mut seen = std::collections::HashSet::new();
        for event in GROK_HOOK_EVENTS {
            assert!(seen.insert(*event), "重复注册事件: {event}");
        }
    }

    /// grok 条目的命令**必须**是不含空格的裸文件名：一旦带上空格（绝对路径或
    /// 事件名参数），grok 就会把它丢给 shell，而 Windows 上具体是 git-bash /
    /// pwsh / powershell / cmd 中的哪一个由环境决定，四家引号语义互斥。
    #[test]
    fn grok_entry_command_never_reaches_a_shell() {
        let entry = build_grok_hook_entry();
        let command = entry["hooks"][0]["command"].as_str().expect("应有命令");
        assert!(command.contains(HOOK_MARKER), "条目须可被 entry_is_miniterm 认出");
        for meta in [' ', '|', '&', ';', '>', '<', '$'] {
            assert!(
                !command.contains(meta),
                "命令 {:?} 含 shell 元字符 {:?}，会被 grok 交给 shell 执行",
                command,
                meta
            );
        }
        assert!(!command.starts_with('~'), "前导 ~ 同样会触发 shell 分支");
        assert!(entry.get("matcher").is_none(), "grok 条目不该写 matcher");
        assert!(entry_is_miniterm(&entry));
    }

    /// 选择性注入的目标解析：显式列表按原样（去重），缺省/空列表回落三家全上
    /// —— 「一键注册」在不勾选任何项时仍是全量，语义不因本功能改变。
    #[test]
    fn targets_default_to_all_and_dedupe_explicit_lists() {
        assert_eq!(resolve_targets(None), HookAgent::ALL.to_vec());
        assert_eq!(resolve_targets(Some(vec![])), HookAgent::ALL.to_vec());
        assert_eq!(
            resolve_targets(Some(vec![HookAgent::Grok])),
            vec![HookAgent::Grok]
        );
        // 重复项会让同一个配置文件被写两遍(幂等但白跑)
        assert_eq!(
            resolve_targets(Some(vec![HookAgent::Codex, HookAgent::Codex, HookAgent::Claude])),
            vec![HookAgent::Codex, HookAgent::Claude]
        );
    }

    /// 未知 agent 必须在 serde 层被拒——静默退化成「全量注册」会往用户
    /// 根本没在用的 CLI 里写配置。
    #[test]
    fn unknown_agent_is_rejected_at_deserialization() {
        assert!(serde_json::from_str::<HookAgent>("\"grok\"").is_ok());
        assert!(serde_json::from_str::<HookAgent>("\"omp\"").is_ok());
        assert!(serde_json::from_str::<HookAgent>("\"pi\"").is_ok());
        assert!(serde_json::from_str::<HookAgent>("\"gemini\"").is_err());
        assert!(serde_json::from_str::<HookAgent>("\"Claude\"").is_err());
    }

    /// 每家的元信息都得齐：key 唯一、事件集非空、展示名不空——
    /// 面板的勾选项与状态徽章全靠它们渲染。
    #[test]
    fn every_agent_exposes_complete_metadata() {
        let mut keys = std::collections::HashSet::new();
        for &agent in HookAgent::ALL {
            assert!(keys.insert(agent.key()), "key 重复: {}", agent.key());
            assert!(!agent.label().is_empty());
            assert!(!agent.events().is_empty(), "{} 的事件集为空", agent.key());
        }
        assert_eq!(keys.len(), 5);
    }

    /// key / 展示名改由 agent 表供给后,与改造前逐字相同,且与 serde 名对齐
    /// (设置页拿 key 当勾选值、后端拿 serde 名反序列化,两边必须是同一个串)。
    #[test]
    fn key_label_come_from_agent_table_unchanged() {
        let expected = [
            (HookAgent::Claude, "claude", "Claude Code"),
            (HookAgent::Codex, "codex", "Codex"),
            (HookAgent::Grok, "grok", "Grok"),
            (HookAgent::Omp, "omp", "oh-my-pi"),
            (HookAgent::Pi, "pi", "pi"),
        ];
        assert_eq!(expected.len(), HookAgent::ALL.len());
        for (agent, key, label) in expected {
            assert_eq!(agent.key(), key);
            assert_eq!(agent.label(), label);
            assert_eq!(HookAgent::from_key(key), Some(agent));
            assert_eq!(
                serde_json::from_str::<HookAgent>(&format!("\"{key}\"")).ok(),
                Some(agent)
            );
        }
        // 反查只认 key 原文(不走 agent 表的别名/宽松口径):勾选值只可能是 key
        assert_eq!(HookAgent::from_key("claude-code"), None);
        assert_eq!(HookAgent::from_key("Claude"), None);
        assert_eq!(HookAgent::from_key("opencode"), None);
    }

    // ---- 进程内 TS 扩展（omp / pi） ----

    /// 从扩展源码里抠出所有 `pi.on("<event>"` 的事件名。
    fn subscribed_events(source: &str) -> std::collections::BTreeSet<String> {
        source
            .match_indices("pi.on(\"")
            .filter_map(|(idx, needle)| {
                let rest = &source[idx + needle.len()..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            .collect()
    }

    /// 模板订阅的事件与声明的事件集必须逐条相等：面板的「N/M」与启动期
    /// 对账都按后者算，两边一漂移，要么永远显示「旧版本」，要么漏掉新事件。
    #[test]
    fn ts_templates_subscribe_exactly_the_declared_events() {
        for ext in [&OMP_EXTENSION, &PI_EXTENSION] {
            let declared: std::collections::BTreeSet<String> =
                ext.events.iter().map(|e| e.to_string()).collect();
            assert_eq!(subscribed_events(ext.source), declared, "{}", ext.tag);
            for shared in [false, true] {
                assert_eq!(
                    ext.events_in_source(ext.source, shared).len(),
                    ext.events.len(),
                    "{} 的当前模板必须被判成「已注册全部事件」",
                    ext.tag
                );
            }
            assert_eq!(template_agent(ext.source), Some(ext.tag));
            let mut seen = std::collections::HashSet::new();
            for event in ext.events {
                assert!(seen.insert(*event), "{} 重复订阅事件: {event}", ext.tag);
            }
        }
    }

    /// 模板上报的事件名都得在 `*_REPORTED_EVENTS` 里（hook_server 按那张表验映射），
    /// 且表里每一个都真的出现在模板中——表比模板宽会让 hook_server 的测试守着
    /// 不存在的事件。
    #[test]
    fn ts_templates_report_only_declared_event_names() {
        for (source, reported) in [
            (OMP_EXTENSION_SOURCE, OMP_REPORTED_EVENTS),
            (PI_EXTENSION_SOURCE, PI_REPORTED_EVENTS),
        ] {
            for event in reported {
                assert!(
                    source.contains(&format!("\"{}\"", event)),
                    "模板里没有上报 {event}"
                );
            }
            // Claude 专有、两家模板都不该冒出来的事件名
            for absent in ["SubagentStart", "SubagentStop", "PostToolBatch"] {
                assert!(
                    !source.contains(&format!("\"{}\"", absent)),
                    "{absent} 出现在模板里却不在上报表里"
                );
            }
        }
        // pi 没有工具审批与重试事件,模板里不该有对应的上报
        for absent in ["PermissionRequest", "PermissionDenied", "Notification"] {
            assert!(
                !PI_EXTENSION_SOURCE.contains(&format!("\"{}\"", absent)),
                "{absent} 出现在 pi 模板里却不在 PI_REPORTED_EVENTS"
            );
        }
    }

    /// pi 模板不能是 omp 模板的翻版:omp 的换会话 / 审批 / 重试事件 pi 一个都不发,
    /// 照抄就等于那几段状态空着;pi 的回合结束只认 agent_settled,omp 没有它。
    #[test]
    fn pi_template_follows_pi_lifecycle_not_omps() {
        let pi_events = subscribed_events(PI_EXTENSION_SOURCE);
        for omp_only in [
            "session_switch",
            "session_branch",
            "tool_approval_requested",
            "tool_approval_resolved",
            "auto_retry_start",
            "auto_compaction_start",
            "auto_compaction_end",
        ] {
            assert!(
                !pi_events.contains(omp_only),
                "pi 模板订阅了 omp 专有事件 {omp_only}"
            );
        }
        assert!(pi_events.contains("agent_settled"));
        assert!(!subscribed_events(OMP_EXTENSION_SOURCE).contains("agent_settled"));
        // 手抄进 pi 目录的 omp 模板(issue #85)要被认成「旧版本」,自愈才会换掉它
        let copied = PI_EXTENSION.events_in_source(OMP_EXTENSION_SOURCE, false);
        assert!(!copied.is_empty() && copied.len() < PI_HOOK_EVENTS.len());
    }

    /// 设了 `PI_CODING_AGENT_DIR`、两家共用一个 `miniterm.ts` 时按模板自报的身份认归属:
    /// 互不把对方的文件当成自己的旧版(E2E 实测过的回归——只用 omp 的用户每次启动
    /// omp 扩展都被 pi 的自愈换成 pi 模板),面板也不会把没注册的那家显示成「旧版本」。
    #[test]
    fn shared_file_is_owned_by_the_template_identity() {
        for (ext, other) in [
            (&OMP_EXTENSION, &PI_EXTENSION),
            (&PI_EXTENSION, &OMP_EXTENSION),
        ] {
            assert!(ext.owns(ext.source, true), "{}", ext.tag);
            assert!(
                !ext.owns(other.source, true),
                "{} 不该认 {} 的文件",
                ext.tag,
                other.tag
            );
            assert!(ext.events_in_source(other.source, true).is_empty());
            // 各在各的目录时,另一家的模板只可能是抄错了地方 → 认成本家旧版
            assert!(ext.owns(other.source, false), "{}", ext.tag);
            // 没有身份行的(不是哪一版模板写的,但带标识)按目录认
            let anonymous = format!("// {HOOK_MARKER}\npi.on(\"session_start\", f);");
            assert!(ext.owns(&anonymous, true) && ext.owns(&anonymous, false));
            assert_eq!(ext.other().tag, other.tag);
        }
        assert_eq!(template_agent("export default () => {}"), None);
    }

    /// 模板的几条硬约束：带标识（注册现状与卸载都按它认）、按 pane/端口环境变量
    /// 定位（缺失即空操作）、只让主会话上报（子代理是同进程里的独立会话）、
    /// 换会话按 clear 收尾、打断按 aborted 上报（不算完成）。
    #[test]
    fn omp_template_keeps_its_contract_strings() {
        for needle in [
            HOOK_MARKER,
            "MINITERM_PTY_ID",
            "MINITERM_HOOK_PORT",
            "hook-server.json",
            "/hook",
            "ctx.mode === \"tui\"",
            "reason: \"clear\"",
            "reason: \"aborted\"",
            "willContinue",
            "127.0.0.1",
        ] {
            assert!(OMP_EXTENSION_SOURCE.contains(needle), "模板缺少 {needle:?}");
        }
        // 只许打回环地址，不许出现别的主机
        assert!(!OMP_EXTENSION_SOURCE.contains("https://"));
    }

    /// pi 模板的硬约束：同 omp 的标识 / 环境变量 / 主会话过滤 / 回环地址;换会话
    /// 打 clear 墓碑、/reload 不报、打断按 aborted 上报、对话框只在回合内点黄灯。
    #[test]
    fn pi_template_keeps_its_contract_strings() {
        for needle in [
            HOOK_MARKER,
            "MINITERM_PTY_ID",
            "MINITERM_HOOK_PORT",
            "hook-server.json",
            "/hook",
            "ctx.mode === \"tui\"",
            "const AGENT = \"pi\"",
            "\"clear\"",
            "reason === \"reload\"",
            "reason: \"aborted\"",
            "event?.aborted === true",
            "!busy",
            "127.0.0.1",
        ] {
            assert!(PI_EXTENSION_SOURCE.contains(needle), "模板缺少 {needle:?}");
        }
        assert!(!PI_EXTENSION_SOURCE.contains("https://"));
    }

    /// 不带标识的同名文件是用户自己的扩展：既不算「已注册」，也绝不能被覆盖。
    #[test]
    fn ts_source_without_marker_is_not_ours() {
        let foreign = "export default function (pi) { pi.on(\"session_start\", () => {}); }";
        for ext in [&OMP_EXTENSION, &PI_EXTENSION] {
            for shared in [false, true] {
                assert!(
                    ext.events_in_source(foreign, shared).is_empty(),
                    "{}",
                    ext.tag
                );
            }
            // 老版本模板（带标识但事件不全）识别成「旧版本 N/M」
            let stale = format!(
                "// {HOOK_MARKER}\nconst AGENT = \"{}\";\npi.on(\"session_start\", f); pi.on(\"agent_end\", g);",
                ext.tag
            );
            let got = ext.events_in_source(&stale, true);
            assert_eq!(got.len(), 2, "{}", ext.tag);
            assert!(got.contains("session_start") && got.contains("agent_end"));
        }
    }

    /// agent 目录的解析口径与 pi 自身一致：`PI_CODING_AGENT_DIR` 压倒一切且认 `~`,
    /// 否则 `~/.pi/agent`。
    #[test]
    fn pi_agent_dir_follows_pi_env_precedence() {
        let home = std::path::Path::new("/home/u");
        assert_eq!(
            pi_agent_dir_from(home, None),
            PathBuf::from("/home/u/.pi/agent")
        );
        assert_eq!(
            pi_agent_dir_from(home, Some("  ")),
            PathBuf::from("/home/u/.pi/agent")
        );
        assert_eq!(
            pi_agent_dir_from(home, Some("/custom/agent")),
            PathBuf::from("/custom/agent")
        );
        assert_eq!(pi_agent_dir_from(home, Some("~")), PathBuf::from("/home/u"));
        assert_eq!(
            pi_agent_dir_from(home, Some("~/dots/pi")),
            PathBuf::from("/home/u/dots/pi")
        );
        // `~user` 不是 home 前缀,原样当路径
        assert_eq!(pi_agent_dir_from(home, Some("~x")), PathBuf::from("~x"));
    }

    /// agent 目录的解析口径与 omp 自身一致：`PI_CODING_AGENT_DIR` 压倒一切，
    /// `PI_CONFIG_DIR` 换根目录名，`PI_PROFILE` 落到 profiles/ 下。
    #[test]
    fn omp_agent_dir_follows_omp_env_precedence() {
        let home = std::path::Path::new("/home/u");
        assert_eq!(
            omp_agent_dir_from(home, None, None, None),
            PathBuf::from("/home/u/.omp/agent")
        );
        assert_eq!(
            omp_agent_dir_from(home, Some("/custom/agent"), Some(".pi"), Some("work")),
            PathBuf::from("/custom/agent")
        );
        assert_eq!(
            omp_agent_dir_from(home, Some("  "), Some(".pi"), None),
            PathBuf::from("/home/u/.pi/agent")
        );
        assert_eq!(
            omp_agent_dir_from(home, None, None, Some("work")),
            PathBuf::from("/home/u/.omp/profiles/work/agent")
        );
        assert_eq!(
            omp_agent_dir_from(home, None, Some(""), Some(" ")),
            PathBuf::from("/home/u/.omp/agent")
        );
    }

    /// 与 Claude 同样的自愈语义：从未注册过的用户不写配置，已是最新的不重复补
    #[test]
    fn grok_sync_is_noop_when_never_registered_or_current() {
        assert!(missing_grok_events(&std::collections::HashSet::new()).is_empty());
        let current: std::collections::HashSet<String> =
            GROK_HOOK_EVENTS.iter().map(|s| s.to_string()).collect();
        assert!(missing_grok_events(&current).is_empty());

        let stale: std::collections::HashSet<String> =
            ["SessionStart", "Stop"].iter().map(|s| s.to_string()).collect();
        assert!(missing_grok_events(&stale).contains(&"SessionEnd"));
    }

    #[test]
    fn codex_registration_includes_authoritative_session_end() {
        assert_eq!(
            CODEX_HOOK_EVENTS
                .iter()
                .filter(|event| **event == "SessionEnd")
                .count(),
            1,
            "Codex 必须且只能注册一次 SessionEnd"
        );

        let entry = build_codex_hook_entry("miniterm-hook", "SessionEnd");
        let command = entry[0]["hooks"][0]["command"]
            .as_str()
            .expect("SessionEnd hook 应包含命令");
        assert!(command.contains("miniterm-hook"));
        assert!(command.ends_with(" SessionEnd"));
    }

    /// Codex CLI 0.152.1 起 feature 名改为 `hooks`。手动片段与一键注册必须
    /// 使用同一现行键；继续展示旧键会让用户照抄后无法启动 hooks。
    #[test]
    fn codex_manual_config_uses_current_hooks_feature() {
        let snippet = get_hook_config_snippet().expect("应生成 hook 配置片段");
        let content = snippet["codex"]["files"][1]["content"]
            .as_str()
            .expect("Codex config.toml 片段应为字符串");

        assert!(content.contains("hooks = true"));
        assert!(
            !content.contains("codex_hooks"),
            "Codex CLI 已不再识别 codex_hooks feature"
        );
    }

    #[test]
    fn codex_feature_config_migrates_legacy_key_and_preserves_user_content() {
        let updated = enable_codex_hooks_feature(
            "# 用户注释\nmodel = \"gpt-5\"\n\n[features]\ncodex_hooks = true\nweb_search = true\n",
        )
        .expect("应迁移 Codex feature");
        let doc = updated
            .parse::<toml_edit::DocumentMut>()
            .expect("迁移结果应是合法 TOML");

        assert_eq!(doc["features"]["hooks"].as_bool(), Some(true));
        assert!(doc["features"].get("codex_hooks").is_none());
        assert_eq!(doc["features"]["web_search"].as_bool(), Some(true));
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
        assert!(updated.contains("# 用户注释"));
    }

    /// 键已经是现行名字时必须原样返回——`ensure_codex_hooks_feature` 拿这个
    /// 相等判断决定写不写盘，启动期自愈全靠它才不会每次启动重写用户配置。
    #[test]
    fn codex_feature_config_is_noop_when_already_current() {
        let current = "# 用户注释\n[features]\nhooks = true\nweb_search = true\n";
        assert_eq!(
            enable_codex_hooks_feature(current).expect("现行配置应可解析"),
            current
        );
    }

    // ---- 卸载摘除(设置页「卸载」与卸载器 `--unregister-hooks` 共用) ----

    /// 每个用例一个独立临时目录,不碰真实 home。
    fn unique_test_dir(label: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("mt-hook-registry-test-{label}-{ts}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn foreign_entry(command: &str) -> Value {
        serde_json::json!({ "matcher": "", "hooks": [{ "type": "command", "command": command }] })
    }

    /// 摘除扫全部事件键(含已从事件表下线的老事件名),只动我们的条目;
    /// 被我们摘空的键删掉,用户原本就空着的数组与其余键序原样保留。
    #[test]
    fn strip_removes_only_ours_across_all_events_and_keeps_key_order() {
        let ours = |event: &str| build_claude_hook_entry(r"C:\mt\miniterm-hook.exe", event);
        let mut config = serde_json::json!({
            "theme": "dark",
            "hooks": {
                "Stop": [foreign_entry("notify-send done"), ours("Stop")],
                "LegacyEvent": [ours("LegacyEvent")],
                "UserEmpty": [],
                "PreToolUse": [ours("PreToolUse")],
                "Notification": [foreign_entry("say hi")]
            }
        });

        let keys = |config: &Value| -> Vec<String> {
            config["hooks"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        let before = keys(&config);

        assert_eq!(strip_miniterm_entries(&mut config), 3);
        assert_eq!(
            config,
            serde_json::json!({
                "theme": "dark",
                "hooks": {
                    "Stop": [foreign_entry("notify-send done")],
                    "UserEmpty": [],
                    "Notification": [foreign_entry("say hi")]
                }
            })
        );
        // 剩下的键保持原有相对顺序(serde_json 开了 preserve_order 时,swap_remove
        // 会把末尾的键挪到被删的位置上;没开时 Map 本就按键名排序,这条自然成立)
        let after = keys(&config);
        let expected: Vec<String> = before.into_iter().filter(|k| after.contains(k)).collect();
        assert_eq!(after, expected, "其余键序不能被打乱");

        // 再摘一次:什么都不剩,也什么都不动
        let snapshot = config.clone();
        assert_eq!(strip_miniterm_entries(&mut config), 0);
        assert_eq!(config, snapshot);
    }

    /// 没有我们条目的文件一个字节都不写(紧凑排版原样保留,不被重排成 pretty);
    /// 有的话摘掉我们的、别人的照留。
    #[test]
    fn removal_leaves_files_without_our_entries_byte_identical() {
        let dir = unique_test_dir("untouched");
        let claude = dir.join("settings.json");
        let codex = dir.join("hooks.json");
        let raw = r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"other-tool"}]}]},"model":"x"}"#;
        std::fs::write(&claude, raw).unwrap();
        std::fs::write(&codex, raw).unwrap();

        assert_eq!(remove_claude_entries_at(&claude).unwrap(), 0);
        assert_eq!(remove_codex_entries_at(&codex).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&claude).unwrap(), raw);
        assert_eq!(std::fs::read_to_string(&codex).unwrap(), raw);

        // 读不懂的文件同样不碰(不冒险改写用户配置)
        std::fs::write(&codex, "{broken").unwrap();
        assert_eq!(remove_codex_entries_at(&codex).unwrap(), 0);
        assert_eq!(std::fs::read_to_string(&codex).unwrap(), "{broken");

        // 文件不在:不建
        let missing = dir.join("absent.json");
        assert_eq!(remove_claude_entries_at(&missing).unwrap(), 0);
        assert!(!missing.exists());

        let mut with_ours: Value = serde_json::from_str(raw).unwrap();
        with_ours["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(build_claude_hook_entry(r"C:\mt\miniterm-hook.exe", "Stop"));
        std::fs::write(&claude, with_ours.to_string()).unwrap();
        assert_eq!(remove_claude_entries_at(&claude).unwrap(), 1);
        let after: Value =
            serde_json::from_str(&std::fs::read_to_string(&claude).unwrap()).unwrap();
        assert_eq!(after, serde_json::from_str::<Value>(raw).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// grok:文件里只剩我们的条目 → 连文件带二进制副本一起删;还有别人的 → 只摘我们的。
    #[test]
    fn grok_removal_deletes_file_and_binary_only_when_all_ours() {
        let dir = unique_test_dir("grok");
        let hooks = dir.join(GROK_HOOK_FILE);
        let bin = dir.join(hook_binary_name());
        let all_ours = serde_json::json!({
            "hooks": { "Stop": [build_grok_hook_entry()], "SessionEnd": [build_grok_hook_entry()] }
        });
        std::fs::write(&hooks, all_ours.to_string()).unwrap();
        std::fs::write(&bin, b"stub").unwrap();

        assert_eq!(remove_grok_entries_at(&hooks, Some(&bin)).unwrap(), 2);
        assert!(!hooks.exists() && !bin.exists());

        let mixed = serde_json::json!({
            "hooks": { "Stop": [build_grok_hook_entry(), foreign_entry("./mine.sh")] }
        });
        std::fs::write(&hooks, mixed.to_string()).unwrap();
        std::fs::write(&bin, b"stub").unwrap();
        assert_eq!(remove_grok_entries_at(&hooks, Some(&bin)).unwrap(), 1);
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&hooks).unwrap()).unwrap();
        assert_eq!(
            after,
            serde_json::json!({ "hooks": { "Stop": [foreign_entry("./mine.sh")] } })
        );
        assert!(bin.exists(), "文件还在用,二进制副本不能删");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// omp / pi:只删本家的扩展文件,用户自己的同名文件、共用目录里另一家的注册都不动。
    #[test]
    fn ts_extension_removal_only_deletes_owned_file() {
        let dir = unique_test_dir("ts-ext");
        let ext = dir.join(EXTENSION_FILE);
        let pi_owns = |shared: bool| move |s: &str| PI_EXTENSION.owns(s, shared);
        assert_eq!(remove_extension_at(&ext, pi_owns(false)).unwrap(), 0);

        std::fs::write(&ext, "export default function (pi) {}").unwrap();
        assert_eq!(remove_extension_at(&ext, pi_owns(false)).unwrap(), 0);
        assert!(ext.exists());

        for source in [OMP_EXTENSION_SOURCE, PI_EXTENSION_SOURCE] {
            std::fs::write(&ext, source).unwrap();
            assert_eq!(remove_extension_at(&ext, pi_owns(false)).unwrap(), 1);
            assert!(!ext.exists());
        }

        // 共用目录:卸载 pi 不能把 omp 的注册删掉
        std::fs::write(&ext, OMP_EXTENSION_SOURCE).unwrap();
        assert_eq!(remove_extension_at(&ext, pi_owns(true)).unwrap(), 0);
        assert!(ext.exists());
        assert_eq!(
            remove_extension_at(&ext, |s| OMP_EXTENSION.owns(s, true)).unwrap(),
            1
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_feature_config_accepts_inline_features_table() {
        let updated =
            enable_codex_hooks_feature("features = { codex_hooks = true, web_search = true }\n")
                .expect("应迁移内联 features 表");
        let doc = updated
            .parse::<toml_edit::DocumentMut>()
            .expect("迁移结果应是合法 TOML");

        assert_eq!(doc["features"]["hooks"].as_bool(), Some(true));
        assert!(doc["features"].get("codex_hooks").is_none());
        assert_eq!(doc["features"]["web_search"].as_bool(), Some(true));
    }
}
