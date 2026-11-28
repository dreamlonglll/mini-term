//! CodeBuddy Code 的 hook 注册/卸载。
//!
//! CodeBuddy 的 hook 配置与 Claude Code 同构（`~/.codebuddy/settings.json` 的
//! `hooks` 段，command 型 hook），所以直接复用 Claude 的条目构造与摘除助手，
//! 只把配置文件路径和事件表换掉。事件名按 CodeBuddy 官方文档的 PascalCase。

use super::*;

/// CodeBuddy 官方支持的事件子集（与 Claude 表取交集，sidecar 不需要改）。
pub(super) const CODEBUDDY_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "SessionEnd",
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "SubagentStop",
    "PreCompact",
    "PostCompact",
];

pub(super) fn codebuddy_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codebuddy").join("settings.json"))
}

fn build_entry(hook_path: &str, event: &str) -> Value {
    let command = if cfg!(windows) {
        format!("\"{}\" {}", hook_path, event)
    } else {
        format!("{} {}", hook_path, event)
    };
    serde_json::json!({
        "matcher": "",
        "hooks": [{ "type": "command", "command": command }]
    })
}

pub(super) fn register_codebuddy_hooks(hook_path: &str) -> Result<String, String> {
    let path = codebuddy_settings_path().ok_or_else(|| "无法获取 home 目录".to_string())?;
    let (added, updated) = claude_settings::update(&path, |settings| {
        if settings.get("hooks").is_none() {
            settings["hooks"] = serde_json::json!({});
        }
        let hooks = settings
            .get_mut("hooks")
            .and_then(|h| h.as_object_mut())
            .ok_or_else(|| "hooks 字段不是对象".to_string())?;
        let mut added = 0;
        let mut updated = 0;
        for event in CODEBUDDY_HOOK_EVENTS {
            let entry = build_entry(hook_path, event);
            if let Some(arr) = hooks.get_mut(*event).and_then(|v| v.as_array_mut()) {
                if let Some(idx) = arr.iter().position(super::entry_is_miniterm) {
                    arr[idx] = entry;
                    updated += 1;
                } else {
                    arr.push(entry);
                    added += 1;
                }
            } else {
                hooks.insert(event.to_string(), serde_json::json!([entry]));
                added += 1;
            }
        }
        Ok::<_, String>((added, updated))
    })?;
    Ok(format!(
        "CodeBuddy: {} 个 hook 已添加, {} 个已更新 (共 {} 个事件)",
        added,
        updated,
        CODEBUDDY_HOOK_EVENTS.len()
    ))
}

pub(super) fn registered_codebuddy_events() -> std::collections::HashSet<String> {
    registered_events_in(codebuddy_settings_path())
}

pub(super) fn remove_codebuddy_entries_at(path: &std::path::Path) -> Result<usize, String> {
    if registered_events_in(Some(path.to_path_buf())).is_empty() {
        return Ok(0);
    }
    claude_settings::update(path, |settings| Ok(strip_miniterm_entries(settings)))
}

pub(super) fn unregister_codebuddy_hooks() -> Result<String, String> {
    let removed = match codebuddy_settings_path() {
        Some(path) => remove_codebuddy_entries_at(&path)?,
        None => 0,
    };
    Ok(if removed == 0 {
        "CodeBuddy: 未发现 mini-term 的 hook 条目，无需卸载".to_string()
    } else {
        format!("CodeBuddy: 已移除 {} 个 hook 条目", removed)
    })
}
