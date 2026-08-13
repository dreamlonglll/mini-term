//! Hook 注册/卸载模块
//!
//! 提供 Tauri commands 用于一键注册/卸载 Claude Code 和 Codex 的 hook 配置，
//! 以及获取配置片段供用户手动粘贴。

use crate::hook_server::{HookState, HookStatusInfo};
use serde_json::Value;
use std::path::PathBuf;
use tauri::AppHandle;

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
    // process_monitor 的 hook 权威模式只接受 SessionEnd 作为会话退出信号。
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

/// 获取 miniterm-hook 二进制的绝对路径
fn get_hook_binary_path() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法获取当前程序路径: {}", e))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "无法获取程序所在目录".to_string())?;

    let hook_path = dir.join(hook_binary_name());
    Ok(hook_path.to_string_lossy().to_string())
}

/// 获取 Claude Code 配置文件路径: ~/.claude/settings.json
fn claude_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
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
pub(crate) fn grok_home() -> Option<PathBuf> {
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
    let settings_path = claude_settings_path().ok_or_else(|| "无法获取 home 目录".to_string())?;

    // 确保 .claude 目录存在
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 .claude 目录失败: {}", e))?;
    }

    // 读取现有配置
    let mut settings: Value = if settings_path.exists() {
        let content = std::fs::read_to_string(&settings_path)
            .map_err(|e| format!("读取 settings.json 失败: {}", e))?;
        serde_json::from_str(&content).map_err(|e| format!("解析 settings.json 失败: {}", e))?
    } else {
        serde_json::json!({})
    };

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

    // 写回配置文件
    let json_str = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("序列化 settings.json 失败: {}", e))?;
    crate::fs::atomic_write(&settings_path, json_str.as_bytes())
        .map_err(|e| format!("写入 settings.json 失败: {}", e))?;

    Ok(format!(
        "Claude Code: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        CLAUDE_HOOK_EVENTS.len()
    ))
}

/// 从 ~/.claude/settings.json 中卸载 miniterm hooks
fn unregister_claude_hooks() -> Result<String, String> {
    let settings_path = match claude_settings_path() {
        Some(p) if p.exists() => p,
        _ => return Ok("Claude Code: settings.json 不存在，无需卸载".to_string()),
    };

    let content = std::fs::read_to_string(&settings_path)
        .map_err(|e| format!("读取 settings.json 失败: {}", e))?;
    let mut settings: Value =
        serde_json::from_str(&content).map_err(|e| format!("解析 settings.json 失败: {}", e))?;

    let mut removed = 0;

    if let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for event in CLAUDE_HOOK_EVENTS {
            if let Some(event_hooks) = hooks.get_mut(*event) {
                if let Some(arr) = event_hooks.as_array_mut() {
                    let before = arr.len();
                    arr.retain(|entry| !entry_is_miniterm(entry));
                    removed += before - arr.len();
                }
            }
        }

        // 清理空的事件数组
        let empty_keys: Vec<String> = hooks
            .iter()
            .filter(|(_, v)| v.as_array().is_some_and(|a| a.is_empty()))
            .map(|(k, _)| k.clone())
            .collect();
        for key in empty_keys {
            hooks.remove(&key);
        }
    }

    let json_str = serde_json::to_string_pretty(&settings)
        .map_err(|e| format!("序列化 settings.json 失败: {}", e))?;
    crate::fs::atomic_write(&settings_path, json_str.as_bytes())
        .map_err(|e| format!("写入 settings.json 失败: {}", e))?;

    Ok(format!("Claude Code: 已移除 {} 个 hook 条目", removed))
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
    registered_events_in(claude_settings_path())
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
    let hook_path = match get_hook_binary_path() {
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

/// 确保 Codex config.toml 中启用了 hooks feature flag
fn ensure_codex_hooks_feature() -> Result<(), String> {
    let config_path = codex_config_path().ok_or_else(|| "无法获取 home 目录".to_string())?;

    // 确保 .codex 目录存在
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 .codex 目录失败: {}", e))?;
    }

    // 读取或创建 config.toml
    let content = if config_path.exists() {
        std::fs::read_to_string(&config_path)
            .map_err(|e| format!("读取 config.toml 失败: {}", e))?
    } else {
        String::new()
    };

    let mut doc: toml_edit::DocumentMut = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("解析 config.toml 失败: {}", e))?;

    // 确保 [features] 段落存在并设置 codex_hooks = true
    if doc.get("features").is_none() {
        doc["features"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    doc["features"]["codex_hooks"] = toml_edit::value(true);

    crate::fs::atomic_write(&config_path, doc.to_string().as_bytes())
        .map_err(|e| format!("写入 config.toml 失败: {}", e))?;

    Ok(())
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
    crate::fs::atomic_write(&hooks_path, json_str.as_bytes())
        .map_err(|e| format!("写入 hooks.json 失败: {}", e))?;

    Ok(format!(
        "Codex: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        CODEX_HOOK_EVENTS.len()
    ))
}

/// 从 ~/.codex/hooks.json 中卸载 miniterm hooks
fn unregister_codex_hooks() -> Result<String, String> {
    let hooks_path = match codex_hooks_path() {
        Some(p) if p.exists() => p,
        _ => return Ok("Codex: hooks.json 不存在，无需卸载".to_string()),
    };

    let content =
        std::fs::read_to_string(&hooks_path).map_err(|e| format!("读取 hooks.json 失败: {}", e))?;
    let mut config: Value =
        serde_json::from_str(&content).map_err(|e| format!("解析 hooks.json 失败: {}", e))?;

    let mut removed = 0;

    if let Some(hooks) = config.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for event in CODEX_HOOK_EVENTS {
            if let Some(event_hooks) = hooks.get_mut(*event) {
                if let Some(arr) = event_hooks.as_array_mut() {
                    let before = arr.len();
                    arr.retain(|entry| !entry_is_miniterm(entry));
                    removed += before - arr.len();
                }
            }
        }

        // 清理空的事件数组
        let empty_keys: Vec<String> = hooks
            .iter()
            .filter(|(_, v)| v.as_array().is_some_and(|a| a.is_empty()))
            .map(|(k, _)| k.clone())
            .collect();
        for key in empty_keys {
            hooks.remove(&key);
        }
    }

    let json_str = serde_json::to_string_pretty(&config)
        .map_err(|e| format!("序列化 hooks.json 失败: {}", e))?;
    crate::fs::atomic_write(&hooks_path, json_str.as_bytes())
        .map_err(|e| format!("写入 hooks.json 失败: {}", e))?;

    Ok(format!("Codex: 已移除 {} 个 hook 条目", removed))
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
    crate::fs::atomic_write(&hooks_path, json_str.as_bytes())
        .map_err(|e| format!("写入 {} 失败: {}", GROK_HOOK_FILE, e))?;

    Ok(format!(
        "Grok: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        GROK_HOOK_EVENTS.len()
    ))
}

/// 从 {grok_home}/hooks/miniterm.json 中卸载 miniterm hooks，
/// 条目清空后连同复制进去的二进制一并删除（那份副本只为本文件服务）
fn unregister_grok_hooks() -> Result<String, String> {
    let hooks_path = match grok_hooks_path() {
        Some(p) if p.exists() => p,
        _ => return Ok(format!("Grok: {} 不存在，无需卸载", GROK_HOOK_FILE)),
    };

    let content = std::fs::read_to_string(&hooks_path)
        .map_err(|e| format!("读取 {} 失败: {}", GROK_HOOK_FILE, e))?;
    let mut config: Value = serde_json::from_str(&content)
        .map_err(|e| format!("解析 {} 失败: {}", GROK_HOOK_FILE, e))?;

    let mut removed = 0;
    if let Some(hooks) = config.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for event in GROK_HOOK_EVENTS {
            if let Some(arr) = hooks.get_mut(*event).and_then(|v| v.as_array_mut()) {
                let before = arr.len();
                arr.retain(|entry| !entry_is_miniterm(entry));
                removed += before - arr.len();
            }
        }
        let empty_keys: Vec<String> = hooks
            .iter()
            .filter(|(_, v)| v.as_array().is_some_and(|a| a.is_empty()))
            .map(|(k, _)| k.clone())
            .collect();
        for key in empty_keys {
            hooks.remove(&key);
        }
    }

    let file_now_empty = config
        .get("hooks")
        .and_then(|h| h.as_object())
        .is_none_or(|h| h.is_empty());

    if file_now_empty {
        // 整个文件都是我们的：直接删掉，别在用户的 hooks 目录留下空壳
        // （sidecar 按该文件是否存在决定要不要丢弃 Claude 兼容层的重复投递）
        std::fs::remove_file(&hooks_path)
            .map_err(|e| format!("删除 {} 失败: {}", GROK_HOOK_FILE, e))?;
        if let Some(bin) = grok_hook_binary_path() {
            if bin.is_file() {
                if let Err(e) = std::fs::remove_file(&bin) {
                    eprintln!("[hook-registry] 删除 grok hook 二进制副本失败: {}", e);
                }
            }
        }
    } else {
        let json_str = serde_json::to_string_pretty(&config)
            .map_err(|e| format!("序列化 {} 失败: {}", GROK_HOOK_FILE, e))?;
        crate::fs::atomic_write(&hooks_path, json_str.as_bytes())
            .map_err(|e| format!("写入 {} 失败: {}", GROK_HOOK_FILE, e))?;
    }

    Ok(format!("Grok: 已移除 {} 个 hook 条目", removed))
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
    let hook_path = match get_hook_binary_path() {
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

// ─── 注册目标 ───

/// 可单独注册/卸载的 CLI。
///
/// 三家的事件集、配置文件位置与命令写法都不同（见各自的 `register_*`），但对外
/// 是同一套动作，所以用本枚举做选择而不是铺三对命令：前端只传一个列表，将来
/// 加第四家时命令签名不变。serde 层拒收未知值——未知 agent 若静默退化成
/// 「全量注册」，会往用户根本没在用的 CLI 里写配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookAgent {
    Claude,
    Codex,
    Grok,
}

impl HookAgent {
    const ALL: &'static [HookAgent] = &[HookAgent::Claude, HookAgent::Codex, HookAgent::Grok];

    fn key(self) -> &'static str {
        match self {
            HookAgent::Claude => "claude",
            HookAgent::Codex => "codex",
            HookAgent::Grok => "grok",
        }
    }

    /// 面板展示名（与 UI 里的品牌写法一致）
    fn label(self) -> &'static str {
        match self {
            HookAgent::Claude => "Claude Code",
            HookAgent::Codex => "Codex",
            HookAgent::Grok => "Grok",
        }
    }

    fn events(self) -> &'static [&'static str] {
        match self {
            HookAgent::Claude => CLAUDE_HOOK_EVENTS,
            HookAgent::Codex => CODEX_HOOK_EVENTS,
            HookAgent::Grok => GROK_HOOK_EVENTS,
        }
    }

    fn registered_events(self) -> std::collections::HashSet<String> {
        match self {
            HookAgent::Claude => registered_claude_events(),
            HookAgent::Codex => registered_codex_events(),
            HookAgent::Grok => registered_grok_events(),
        }
    }

    /// 配置文件路径的展示形式（`~` 缩写，面板里直接给用户看到写去了哪）
    fn display_path(self) -> String {
        let raw = match self {
            HookAgent::Claude => claude_settings_path(),
            HookAgent::Codex => codex_hooks_path(),
            HookAgent::Grok => grok_hooks_path(),
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
        }
    }

    fn unregister(self) -> Result<String, String> {
        match self {
            HookAgent::Claude => unregister_claude_hooks(),
            HookAgent::Codex => unregister_codex_hooks(),
            HookAgent::Grok => unregister_grok_hooks(),
        }
    }
}

/// 入参缺省 / 空列表时的目标：三家全上，保持「一键注册」的原有语义。
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
    /// 与 `HookAgent` 的 serde 表示一致，前端按它回传选择
    pub agent: String,
    pub label: String,
    /// 配置文件路径（`~` 缩写）
    pub file: String,
    /// 该文件里属于 mini-term 的事件条目数
    pub registered: usize,
    /// 当前版本应注册的事件总数；`0 < registered < total` = 老用户的旧事件集
    pub total: usize,
}

// ─── Tauri Commands ───

/// 注册 AI hooks。`agents` 缺省 = 三家全注册（保持「一键注册」的原语义）。
#[tauri::command]
pub fn register_ai_hooks(
    _app: AppHandle,
    agents: Option<Vec<HookAgent>>,
) -> Result<String, String> {
    let hook_path = get_hook_binary_path()?;
    let results: Vec<String> = resolve_targets(agents)
        .into_iter()
        .map(|agent| match agent.register(&hook_path) {
            Ok(msg) => msg,
            // 单家失败不打断其余：三个配置文件互不相干，一家读不动不该
            // 让另外两家也注册不上
            Err(e) => format!("{} 注册失败: {}", agent.label(), e),
        })
        .collect();
    Ok(results.join("\n"))
}

/// 卸载 AI hooks。`agents` 缺省 = 三家全卸载。
#[tauri::command]
pub fn unregister_ai_hooks(
    _app: AppHandle,
    agents: Option<Vec<HookAgent>>,
) -> Result<String, String> {
    let results: Vec<String> = resolve_targets(agents)
        .into_iter()
        .map(|agent| match agent.unregister() {
            Ok(msg) => msg,
            Err(e) => format!("{} 卸载失败: {}", agent.label(), e),
        })
        .collect();
    Ok(results.join("\n"))
}

/// 三家各自的注册现状（面板据此定默认勾选、显示状态徽章）。
#[tauri::command]
pub fn get_ai_hook_registrations(_app: AppHandle) -> Vec<HookRegistrationInfo> {
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

/// 获取 hook 配置片段供用户手动粘贴（结构化返回）
#[tauri::command]
pub fn get_hook_config_snippet(_app: AppHandle) -> Result<Value, String> {
    let hook_path = get_hook_binary_path()?;

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
                    "content": "[features]\ncodex_hooks = true"
                }
            ]
        }
    }))
}

/// 获取当前 hook 状态信息
#[tauri::command]
pub fn get_hook_status(
    _app: AppHandle,
    hook_state: tauri::State<'_, HookState>,
) -> Result<HookStatusInfo, String> {
    Ok(HookStatusInfo {
        port: hook_state.get_port(),
        running: hook_state.is_server_running(),
    })
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
    /// —— 「一键注册」在前端不勾选任何项时仍是全量，语义不因本功能改变。
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
        assert_eq!(keys.len(), 3);
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
}
