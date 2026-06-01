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
const CLAUDE_HOOK_EVENTS: &[&str] = &[
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
];

/// Codex 需要注册的 hook 事件列表
const CODEX_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "PermissionRequest",
];

/// 获取 miniterm-hook 二进制的绝对路径
fn get_hook_binary_path() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法获取当前程序路径: {}", e))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "无法获取程序所在目录".to_string())?;

    let hook_name = if cfg!(windows) {
        "miniterm-hook.exe"
    } else {
        "miniterm-hook"
    };

    let hook_path = dir.join(hook_name);
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
                let existing_idx = arr.iter().position(|entry| {
                    entry
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|hooks_arr| {
                            hooks_arr.iter().any(|h| {
                                h.get("command")
                                    .and_then(|c| c.as_str())
                                    .map(|c| c.contains(HOOK_MARKER))
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                });

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
                    arr.retain(|entry| {
                        !entry
                            .get("hooks")
                            .and_then(|h| h.as_array())
                            .map(|hooks_arr| {
                                hooks_arr.iter().any(|h| {
                                    h.get("command")
                                        .and_then(|c| c.as_str())
                                        .map(|c| c.contains(HOOK_MARKER))
                                        .unwrap_or(false)
                                })
                            })
                            .unwrap_or(false)
                    });
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
                let existing_idx = arr.iter().position(|entry| {
                    entry
                        .get("hooks")
                        .and_then(|h| h.as_array())
                        .map(|hooks_arr| {
                            hooks_arr.iter().any(|h| {
                                h.get("command")
                                    .and_then(|c| c.as_str())
                                    .map(|c| c.contains(HOOK_MARKER))
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                });

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
                    arr.retain(|entry| {
                        !entry
                            .get("hooks")
                            .and_then(|h| h.as_array())
                            .map(|hooks_arr| {
                                hooks_arr.iter().any(|h| {
                                    h.get("command")
                                        .and_then(|c| c.as_str())
                                        .map(|c| c.contains(HOOK_MARKER))
                                        .unwrap_or(false)
                                })
                            })
                            .unwrap_or(false)
                    });
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

// ─── Tauri Commands ───

/// 注册 AI hooks（Claude Code + Codex）
#[tauri::command]
pub fn register_ai_hooks(_app: AppHandle) -> Result<String, String> {
    let hook_path = get_hook_binary_path()?;

    let mut results = Vec::new();

    match register_claude_hooks(&hook_path) {
        Ok(msg) => results.push(msg),
        Err(e) => results.push(format!("Claude Code 注册失败: {}", e)),
    }

    match register_codex_hooks(&hook_path) {
        Ok(msg) => results.push(msg),
        Err(e) => results.push(format!("Codex 注册失败: {}", e)),
    }

    Ok(results.join("\n"))
}

/// 卸载 AI hooks（Claude Code + Codex）
#[tauri::command]
pub fn unregister_ai_hooks(_app: AppHandle) -> Result<String, String> {
    let mut results = Vec::new();

    match unregister_claude_hooks() {
        Ok(msg) => results.push(msg),
        Err(e) => results.push(format!("Claude Code 卸载失败: {}", e)),
    }

    match unregister_codex_hooks() {
        Ok(msg) => results.push(msg),
        Err(e) => results.push(format!("Codex 卸载失败: {}", e)),
    }

    Ok(results.join("\n"))
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

    Ok(serde_json::json!({
        "claude": {
            "file": "~/.claude/settings.json",
            "content": claude_str
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
